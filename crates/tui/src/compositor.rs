//! Production screen compositor: turns [`AppState`] plus a live
//! [`Transcript`]/[`TranscriptViewport`]/composer text/[`StatusChrome`] into
//! a fixed-size grid of text rows, using the existing layout/panel/status
//! machinery unchanged.
//!
//! This is the single place that decides "what goes where" on screen. It is
//! frontend-agnostic: it never touches a real terminal — a caller (the
//! production `rapid` binary, or a test) decides how to turn [`Screen`]'s
//! rows into either a real paint or a golden-string assertion. This module
//! is a *production-facing generalization* of the layout/paint-order
//! algorithm `crates/tui/tests/ui_snapshots.rs`'s own test-local
//! `paint_screen`/`CellGrid` already proved correct — this crate's own
//! `#[cfg(test)]` module below covers the production entry points directly.
//! `ui_snapshots.rs` was deliberately left as its own, separate
//! implementation rather than retrofitted to call this one: its transcript
//! rows are formatted `"{kind}|{text}"` (a debug-golden convenience so its
//! own assertions can check block *kind* and text independently), which
//! this module's real, production-facing transcript formatting
//! (`render_block_parts`, plain user-facing text with no kind prefix) does
//! not produce — reconciling the two formats without risking a large,
//! separate rewrite of that file's many existing golden constants was
//! judged out of scope for this change. The *layout/dispatch* shape (which
//! region gets which content, in what order) is the same shape in both
//! places; only the transcript text format differs, for a reason specific
//! to each caller.
//!
//! Route content is honest about what `AppState` actually carries: `Agents`
//! and `Goals` project real state (`AppState::agents`/`AppState::goals`).
//! `Diff`, `Context`, `Memory`, `Jobs`, `Approvals`, `Graph`, `Computer`,
//! `Resources`, and `Models` render empty sidebar content — each of those
//! panels' own view-model constructor needs data `AppState` does not carry
//! today (a live `ContextPacket`, memory observations, span/job detail
//! records, full approval capability/risk/policy specs, etc.), so rendering
//! anything for them here would mean inventing plausible-looking content
//! from nothing. This mirrors the precedent this crate's own reference
//! compositor already established for `Graph`/`Computer`/`Resources`/
//! `Models` ("no sidebar model yet") — extended honestly to the other
//! routes that turn out to have the same gap, rather than pretending only
//! four routes have it.

use protocol::JobId;

use crate::layout::{LayoutRects, Rect, UiMode, compute_layout_with_composer};
use crate::panels::agents::{AgentsSelection, AgentsViewModel};
use crate::panels::goals::GoalViewModel;
use crate::sanitize::sanitize_untrusted;
use crate::state::{AppState, CancellationToken, JobLogView, JobProjection, UiRoute};
use crate::status::{StatusChrome, render_status_with};
use crate::transcript::{Transcript, TranscriptViewport};

/// One painted frame: a fixed-size grid of text rows. Never itself talks to
/// a terminal — see this module's own doc comment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Screen {
    width: u16,
    height: u16,
    rows: Vec<String>,
}

impl Screen {
    fn blank(width: u16, height: u16) -> Self {
        let w = usize::from(width);
        Self {
            width,
            height,
            rows: vec![" ".repeat(w); usize::from(height)],
        }
    }

    fn paint_lines(&mut self, rect: Rect, lines: &[String]) {
        if rect.is_empty() || self.width == 0 || self.height == 0 {
            return;
        }
        let x = usize::from(rect.x());
        let y = usize::from(rect.y());
        let w = usize::from(rect.width());
        let h = usize::from(rect.height());
        let grid_width = usize::from(self.width);
        let grid_height = usize::from(self.height);
        for row in 0..h {
            let dest_y = y + row;
            if dest_y >= grid_height {
                break;
            }
            let src = lines.get(row).map(String::as_str).unwrap_or("");
            let fitted = fit_width(src, w);
            let dest_x = x.min(grid_width);
            let end = (dest_x + w).min(grid_width);
            if dest_x >= end {
                continue;
            }
            let mut chars: Vec<char> = self.rows[dest_y].chars().collect();
            let piece: Vec<char> = fitted.chars().take(end - dest_x).collect();
            for (offset, ch) in piece.into_iter().enumerate() {
                if dest_x + offset < chars.len() {
                    chars[dest_x + offset] = ch;
                }
            }
            self.rows[dest_y] = chars.into_iter().collect();
        }
    }

    /// Row `y` (0-indexed from the top), already fitted to the screen
    /// width. `None` if `y` is out of range.
    pub fn row(&self, y: u16) -> Option<&str> {
        self.rows.get(usize::from(y)).map(String::as_str)
    }

    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    /// Rows joined by `\n` — a convenience for golden-string assertions.
    pub fn snapshot(&self) -> String {
        self.rows.join("\n")
    }
}

/// The [`UiMode`] a screen of this size/state/modal-openness should use.
/// Sidebar content is requested whenever the route isn't the bare
/// transcript, regardless of whether that route currently has real content
/// to show (an empty sidebar is still a real, allocated region — see this
/// module's own doc comment on which routes render empty).
pub fn ui_mode_for(state: &AppState, modal_open: bool) -> UiMode {
    match (state.route() != UiRoute::Transcript, modal_open) {
        (false, false) => UiMode::Transcript,
        (true, false) => UiMode::Sidebar,
        (false, true) => UiMode::Modal,
        (true, true) => UiMode::SidebarModal,
    }
}

/// [`compute_layout_with_composer`] using the mode this state/modal
/// combination implies.
pub fn compute_screen_layout(
    state: &AppState,
    area: Rect,
    composer_lines: u16,
    modal_open: bool,
) -> LayoutRects {
    compute_layout_with_composer(area, ui_mode_for(state, modal_open), composer_lines)
}

/// Route-selected sidebar content, `width`x`height` lines. See this
/// module's own doc comment for which routes have real content today.
pub fn sidebar_lines(
    route: UiRoute,
    state: &AppState,
    width: u16,
    height: u16,
    cancel: &CancellationToken,
) -> Vec<String> {
    match route {
        UiRoute::Transcript => Vec::new(),
        UiRoute::Agents => agent_panel_lines(state, width, height, cancel),
        UiRoute::Goals => goal_lines(state, width, height),
        UiRoute::Jobs => job_lines(state, width, height),
        UiRoute::Approvals => approval_lines(state, width, height),
        UiRoute::Models => model_lines(state, width, height),
        UiRoute::Memory => memory_lines(state, width, height),
        UiRoute::Context => context_lines(state, width, height),
        UiRoute::Diff => diff_lines(state, width, height),
        UiRoute::Graph | UiRoute::Computer | UiRoute::Resources => Vec::new(),
    }
}

/// The agents panel: the session's agents, then the agent types a spawn
/// may name in the rows the agents leave — the types never take a line
/// the agents would have painted. The types are cut to a trailing "… n
/// more" line counting every type the host synced, never a bare header;
/// a single spare row says how many there are.
fn agent_panel_lines(
    state: &AppState,
    width: u16,
    height: u16,
    cancel: &CancellationToken,
) -> Vec<String> {
    let mut lines = if state.agents().is_empty() && !state.agent_types().is_empty() {
        // No agent to describe: the types have the panel.
        Vec::new()
    } else {
        AgentsViewModel::from_state(state, &[], AgentsSelection::default(), cancel)
            .map(|model| model.render(width, height).lines().to_vec())
            .unwrap_or_default()
    };
    lines.truncate(usize::from(height));
    let rows = state.agent_types();
    let total = state.agent_types_total().max(rows.len());
    let room = usize::from(height) - lines.len();
    if rows.is_empty() || room == 0 {
        return lines;
    }
    if room == 1 {
        // No room for the list: say it is there.
        lines.push(format!("agent types: {total} (rapid agents list)"));
        return lines;
    }
    lines.push("agent types (task_spawn type=<id>)".to_owned());
    let room = room - 1;
    let fits = if total <= room {
        rows.len()
    } else {
        room.saturating_sub(1).min(rows.len())
    };
    for row in &rows[..fits] {
        lines.push(format!(
            "  {}",
            crate::sanitize::sanitize_untrusted(&row.line)
        ));
    }
    if fits < total {
        lines.push(format!("  … {} more (rapid agents list)", total - fits));
    }
    lines
}

/// Whether [`sidebar_lines`] can produce content for `route`, as opposed to
/// switching to a panel that paints nothing.
///
/// The authority on this deliberately lives *here*, beside the match that
/// decides it, and mirrors that match arm for arm — adding a `UiRoute`
/// variant fails to compile in both places at once. It exists because
/// "`Inspector::route()` returns `Some`" is emphatically **not** the same
/// fact: nine of the twelve routes resolve to a panel with no content, and a
/// frontend that treated a route as evidence of a working command told users
/// `/diff`, `/memory` and `/jobs` were fine when they open an empty sidebar
/// — on a narrow terminal, one that takes the whole transcript rect.
pub const fn route_renders_content(route: UiRoute) -> bool {
    match route {
        // The transcript is the default view, not an inspector panel.
        UiRoute::Transcript => false,
        UiRoute::Agents
        | UiRoute::Goals
        | UiRoute::Jobs
        | UiRoute::Approvals
        | UiRoute::Models
        | UiRoute::Memory
        | UiRoute::Context
        | UiRoute::Diff => true,
        UiRoute::Graph | UiRoute::Computer | UiRoute::Resources => false,
    }
}

/// One line per goal: statement, lifecycle, turns/tokens against budget.
/// `GoalViewModel` (unlike its sibling panels) has no `.render()`/`Frame`
/// of its own yet — this is a deliberately minimal, local formatting of its
/// existing row data, not a new panel design; see `GoalRow`'s own fields.
fn goal_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let model = GoalViewModel::from_state(state);
    let mut lines: Vec<String> = model
        .rows()
        .iter()
        .map(|row| {
            let marker = if Some(row.id()) == model.selected() {
                ">"
            } else {
                " "
            };
            let lifecycle = format!("{:?}", row.lifecycle()).to_lowercase();
            let turns = match row.max_turns() {
                Some(max) => format!("{}/{max}", row.turns()),
                None => row.turns().to_string(),
            };
            let tokens = match row.max_tokens() {
                Some(max) => format!("{}/{max}", row.tokens()),
                None => row.tokens().to_string(),
            };
            format!(
                "{marker} {} [{lifecycle}] turns:{turns} tokens:{tokens}",
                row.statement()
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("no goal".to_owned());
    }
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// The `/jobs` panel: the job rows `reduce` already keeps.
///
/// The projection was populated from `job.*` events all along
/// (`upsert_job`); only the rendering was missing, so the route switched to a
/// panel that painted nothing — and on a narrow terminal an empty panel takes
/// the whole transcript rect. Rows are ordered by id (the `BTreeMap`'s own
/// The `/jobs` panel, in the view the command asked for.
///
/// Three views, one function, because they answer the same question at
/// different resolutions: the list ("what is running"), one job ("what is
/// *that* one doing"), and its output ("what did it print"). `/jobs show
/// <id>` and `/jobs logs <id>` parsed an id from the beginning and every
/// consumer dropped it, so both opened the same unfiltered list as a bare
/// `/jobs` — the panel could not tell it had been asked about one job.
///
/// Log text is process output and gets [`sanitize_untrusted`] like every
/// other untrusted string the compositor paints: a build log is exactly the
/// kind of content that carries escape sequences, and it must not be able
/// to repaint the terminal around it.
fn job_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut lines = match (state.selected_job(), state.job_logs()) {
        // A logs page belonging to the selected job.
        (Some(selected), Some(page)) if page.job() == selected => job_log_lines(state, page),
        // One job, named by `/jobs show <id>`.
        (Some(selected), _) => job_detail_lines(state, selected),
        (None, _) => job_list_lines(state, usize::from(height)),
    };
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// The list view within `height` rows: the notices first (what background
/// work told the user, newest first), then the session's loops (each with
/// its next fire and expiry; cut with a count, never a header alone), then
/// one row per job with what room is left — jobs give way first.
fn job_list_lines(state: &AppState, height: usize) -> Vec<String> {
    let mut notices: Vec<String> = Vec::new();
    let recorded = state.notifications();
    if !recorded.is_empty() {
        notices.push("notifications".to_owned());
        for notice in recorded.iter().rev().take(MAX_PANEL_NOTICES) {
            // Model output: sanitized like any untrusted text the panel
            // shows, so it cannot drive the terminal.
            let first = notice.text().lines().next().unwrap_or_default();
            notices.push(format!(
                "  {}: {}",
                crate::sanitize::sanitize_untrusted(notice.source()),
                crate::sanitize::sanitize_untrusted(first)
            ));
        }
    }
    notices.truncate(height);
    let mut room = height - notices.len();
    let mut loops: Vec<String> = Vec::new();
    let rows = state.loops();
    if !rows.is_empty() && room >= 2 {
        loops.push("loops (/loop rm <id>)".to_owned());
        room -= 1;
        let fits = if rows.len() <= room {
            rows.len()
        } else {
            room.saturating_sub(1)
        };
        for row in &rows[..fits] {
            loops.push(format!(
                "  {}",
                crate::sanitize::sanitize_untrusted(&row.line)
            ));
        }
        if fits < rows.len() {
            loops.push(format!("  … {} more (/loop lists them)", rows.len() - fits));
        }
        room -= loops.len() - 1;
    } else if !rows.is_empty() && room == 1 {
        loops.push(format!("loops: {} (/loop lists them)", rows.len()));
        room = 0;
    }
    let mut jobs: Vec<String> = state.jobs().values().map(job_row).collect();
    if jobs.is_empty() {
        jobs.push("no jobs".to_owned());
    }
    jobs.truncate(room);
    let mut lines = jobs;
    lines.extend(loops);
    lines.extend(notices);
    lines
}

/// Notices the jobs panel lists under its rows.
const MAX_PANEL_NOTICES: usize = 5;

/// The row `/jobs show <id>` asked for, or why there is none.
fn job_detail_lines(state: &AppState, selected: JobId) -> Vec<String> {
    match state.jobs().get(&selected) {
        Some(job) => vec![job_row(job), format!("id {selected}")],
        // Naming a job that is not in this session's projection is a real
        // outcome (a stale id, another session's job), and saying so beats
        // silently painting the whole list as though nothing was asked.
        None => vec![format!("no job {selected} in this session")],
    }
}

/// The captured output of the job `/jobs logs <id>` named.
fn job_log_lines(state: &AppState, page: &JobLogView) -> Vec<String> {
    let mut lines = Vec::new();
    match state.jobs().get(&page.job()) {
        Some(job) => lines.push(job_row(job)),
        None => lines.push(format!("job {}", page.job())),
    }
    if page.truncated() {
        lines.push("output truncated at the capture limit".to_owned());
    }
    if page.lines().is_empty() {
        // True whether the job printed nothing or was started by an earlier
        // process whose spool died with it: either way nothing was captured
        // here, and claiming the job produced no output would not be.
        lines.push("no output captured in this process".to_owned());
        return lines;
    }
    lines.extend(
        page.lines()
            .iter()
            .map(|line| sanitize_untrusted(line).into_owned()),
    );
    lines
}

/// One job as a row: the command when the producer recorded one, since a
/// list of UUIDs cannot tell a reader which row is the test run they are
/// waiting on.
fn job_row(job: &JobProjection) -> String {
    let lifecycle = format!("{:?}", job.state()).to_lowercase();
    // The name a user can type first, then what it is. The `job-N` handle
    // is what the transcript calls it and what `/jobs cancel` accepts;
    // absent one (an older ledger), the id's random tail, which the
    // commands accept too.
    let name = match job.handle() {
        Some(handle) => handle.to_owned(),
        None => short_id(&job.id().to_string()),
    };
    let what = job.command().unwrap_or("");
    let head = if what.is_empty() {
        name
    } else {
        format!("{name}  {what}")
    };
    match job.exit_status() {
        Some(status) => format!("{head} [{lifecycle}] exit:{status}"),
        None => format!("{head} [{lifecycle}]"),
    }
}

/// The last dash-group of a UUID: the random tail of a UUIDv7, which is
/// the part that distinguishes ids minted close together — and therefore
/// the part `IdResolver` matches. Same convention as the agents panel.
fn short_id(raw: &str) -> String {
    raw.rsplit('-').next().unwrap_or(raw).to_owned()
}

/// The `/approvals` panel, on the same footing as [`job_lines`].
///
/// A resolved approval shows what it was resolved *as*: "resolved" alone
/// cannot distinguish an approval that was granted from one that was refused,
/// which is the single fact a reader opens this panel for.
fn approval_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut lines: Vec<String> = state
        .approvals()
        .values()
        .map(|approval| {
            let lifecycle = format!("{:?}", approval.state()).to_lowercase();
            match approval.decision() {
                Some(decision) => format!(
                    "{} [{lifecycle}] {}",
                    approval.id(),
                    format!("{decision:?}").to_lowercase()
                ),
                None => format!("{} [{lifecycle}]", approval.id()),
            }
        })
        .collect();
    if lines.is_empty() {
        lines.push("no approvals".to_owned());
    }
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// The `/models` panel: which model this session resolved, and what else is
/// configured to fall back to.
///
/// Rows come from [`AppState::models`], projected by the host from
/// `[model.<id>]` tables — configuration is read from files and environment,
/// not from session history, so there is no kernel event to carry it and
/// `LocalUiEvent::SyncModels` is how it arrives. The active model is marked
/// the way `goal_lines` marks its selection, and a fallback shows its
/// position, because "what runs if this provider fails, and in what order"
/// is the question the panel exists to answer.
fn model_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut lines: Vec<String> = state
        .models()
        .iter()
        .map(|row| {
            let marker = if row.active { ">" } else { " " };
            let mut line = format!("{marker} {} {}/{}", row.id, row.provider, row.model);
            if let Some(window) = row.context_window {
                line.push_str(&format!(" ctx:{window}"));
            }
            if let Some(rank) = row.fallback_rank {
                line.push_str(&format!(" fallback#{}", rank + 1));
            }
            line
        })
        .collect();
    if lines.is_empty() {
        lines.push("no models configured".to_owned());
    }
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// The `/memory` panel: the project memory index, exactly as the model
/// receives it.
///
/// Rendered from the same bounded text `host::load_memory_index` hands the
/// model — not a second read with different bounds — so what a user sees
/// here is what is actually in the model's context, which is the only
/// version of this panel worth having.
///
/// `MEMORY.md` is repository content and therefore untrusted: a clone can
/// carry escape sequences that would move the cursor or repaint the frame,
/// so every line goes through `sanitize_untrusted` before it is fitted.
fn memory_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut lines: Vec<String> = state
        .memory()
        .iter()
        .map(|line| sanitize_untrusted(line).into_owned())
        .collect();
    if lines.is_empty() {
        lines.push("no .rapidlm/MEMORY.md in this project".to_owned());
    }
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// The `/context` panel: what the last turn's compiled context was made of.
///
/// The totals answer "how full is the window"; the per-class rows answer the
/// question a reader actually has when it is nearly full — *which* class is
/// consuming it. Both come from the same `context.compiled` event, so the
/// panel and the status bar's `ctx:` item can never disagree.
///
/// Classes are shown in the compiler's own order rather than sorted by size,
/// so a row does not move between redraws while a user is reading it.
fn context_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    // One panel, two tabs: `/context` shows the window, `/usage` what the
    // session's model steps consumed and cost (SEAM-07).
    let tab = state.context_tab();
    let header = match tab {
        crate::state::ContextTab::Window => "[context]  usage",
        crate::state::ContextTab::Usage => " context  [usage]",
    };
    let mut lines = vec![header.to_owned()];
    let body_height = height.saturating_sub(1);
    let body = match tab {
        crate::state::ContextTab::Usage => usage_lines(state.session_usage()),
        crate::state::ContextTab::Window => {
            // A search asked a different question than "how full is the
            // window", so it gets the panel while it is open.
            if let Some(found) = state.context_search() {
                context_search_lines(found, width, body_height)
            } else {
                window_lines(state)
            }
        }
    };
    lines.extend(body);
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// The window tab: the totals, then a legend — each class's share of the
/// window and the free share — whose percentages sum to exactly 100.
fn window_lines(state: &AppState) -> Vec<String> {
    let Some((used, limit)) = state.context_usage() else {
        return vec!["no turn has compiled a context yet".to_owned()];
    };
    let mut lines = vec![format!("total {used}/{limit}")];
    let partitions = state.context_partitions();
    let mut parts: Vec<u64> = partitions.iter().map(|partition| partition.used).collect();
    let taken: u64 = parts.iter().sum();
    parts.push(limit.saturating_sub(taken));
    let shares = percent_shares(&parts);
    for (partition, share) in partitions.iter().zip(&shares) {
        lines.push(format!(
            "  {} {}/{} {share}%",
            partition.class, partition.used, partition.cap
        ));
    }
    if let (Some(free), Some(share)) = (parts.last(), shares.last())
        && !partitions.is_empty()
    {
        lines.push(format!("  free {free} {share}%"));
    }
    lines
}

/// Whole percentages of `parts`' total that sum to exactly 100 (largest
/// remainder); all zero when the total is.
fn percent_shares(parts: &[u64]) -> Vec<u64> {
    let total: u128 = parts.iter().map(|&part| u128::from(part)).sum();
    if total == 0 {
        return vec![0; parts.len()];
    }
    let exact: Vec<u128> = parts.iter().map(|&part| u128::from(part) * 100).collect();
    let mut shares: Vec<u64> = exact
        .iter()
        .map(|value| u64::try_from(value / total).unwrap_or(100))
        .collect();
    let mut left = 100u64.saturating_sub(shares.iter().sum());
    let mut order: Vec<usize> = (0..parts.len()).collect();
    // Largest remainder first; ties to the earlier row, so the legend is
    // stable between redraws.
    order.sort_by(|&a, &b| (exact[b] % total).cmp(&(exact[a] % total)).then(a.cmp(&b)));
    for index in order {
        if left == 0 {
            break;
        }
        shares[index] += 1;
        left -= 1;
    }
    shares
}

/// The usage tab: the session's model steps, by the rule `rapid usage`
/// reduces them with — an unknown split or cost is `unknown`, never zero.
fn usage_lines(usage: &crate::state::SessionUsage) -> Vec<String> {
    if usage.steps == 0 {
        return vec!["no model step recorded yet".to_owned()];
    }
    let known = |value: Option<u64>| value.map_or_else(|| "unknown".to_owned(), |v| v.to_string());
    let usd = |micros: u64| format!("${}.{:06}", micros / 1_000_000, micros % 1_000_000);
    let cost = match usage.cost_usd_micros() {
        Some(micros) => usd(micros),
        None if usage.cost_known_usd_micros > 0 => {
            format!("unknown (>= {})", usd(usage.cost_known_usd_micros))
        }
        None => "unknown".to_owned(),
    };
    let basis = if usage.cost_unknown_steps > 0 {
        "unknown"
    } else if usage.tokens_estimated_steps > 0 {
        "estimated"
    } else {
        "reported"
    };
    vec![
        format!("turns {}  steps {}", usage.turns, usage.steps),
        format!("tokens {}", usage.tokens),
        format!("  input {}", known(usage.input_tokens)),
        format!("  output {}", known(usage.output_tokens)),
        format!("  cached {}", known(usage.cached_tokens)),
        format!("cost {cost}"),
        format!("basis {basis}"),
        "rapid usage: per turn, and across sessions".to_owned(),
    ]
}

/// The `/context search <query>` view: what proactive retrieval would put
/// in front of the model for that query.
///
/// These are the blocks the *same* `context_retrieval::retrieve` call the
/// turn path makes would surface, so the panel answers "what would the
/// agent see if I asked this" rather than describing a separate index.
///
/// Locators are repository paths, so they render through
/// [`sanitize_untrusted`] like every other untrusted string.
fn context_search_lines(
    found: &crate::state::ContextSearchView,
    width: u16,
    height: u16,
) -> Vec<String> {
    let query = sanitize_untrusted(found.query());
    let mut lines = vec![match found.outcome() {
        crate::state::ContextSearchOutcome::Searched => {
            format!("search {query} — {} block(s)", found.hits().len())
        }
        // Never reported as "no matches": the project was never walked, and
        // saying so names the thing the user can actually change.
        crate::state::ContextSearchOutcome::Untrusted => {
            format!("search {query} — project is not trusted, nothing indexed")
        }
    }];
    for hit in found.hits() {
        lines.push(format!(
            "  {} {} bytes",
            sanitize_untrusted(&hit.locator),
            hit.bytes
        ));
    }
    if lines.len() == 1
        && matches!(
            found.outcome(),
            crate::state::ContextSearchOutcome::Searched
        )
    {
        lines.push("  nothing retrieved for this query".to_owned());
    }
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// The `/diff` panel: what this session changed, as hunks where it can.
///
/// Each file gets its row — path, line counts, write count — and then the
/// unified hunks of its latest write when the producer computed them. The
/// counts stay on every row because they are true for every write; the
/// hunks appear only when they are, so a binary file or a rewrite past the
/// differ's bounds shows counts alone rather than an invented `+n/-m`.
///
/// Every line is file content or a path from a tool call, so it renders
/// through `sanitize_untrusted` like every other untrusted string — a
/// source file is exactly where an escape sequence can be waiting.
fn diff_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    // The notice belongs here, not in the transcript: opening this panel
    // replaces the transcript, so a message printed there would be
    // invisible exactly when it matters.
    if let Some(agent) = state.diff_agent() {
        // Two lines because one is not survivable: an `AgentId` is 36
        // characters, so a combined notice is cut mid-sentence by
        // `fit_width` on an 80-column panel and loses the very thing it
        // exists to say.
        lines.push(format!("--agent {agent}"));
        lines.push("  no per-agent attribution; showing every change".to_owned());
    }
    if state.changed_files().is_empty() {
        lines.push("no files changed in this session".to_owned());
    }
    for file in state.changed_files().values() {
        let path = sanitize_untrusted(&file.path);
        let change = match file.lines_before {
            None => format!("new, {} lines", file.lines_after),
            Some(before) => format!("{before} -> {} lines", file.lines_after),
        };
        if file.writes > 1 {
            lines.push(format!(
                "{path}  {change} ({} writes, latest shown)",
                file.writes
            ));
        } else {
            lines.push(format!("{path}  {change}"));
        }
        if let Some(hunks) = file.hunks.as_deref() {
            lines.extend(
                hunks
                    .lines()
                    .map(|line| format!("  {}", sanitize_untrusted(line))),
            );
        }
        // Stop folding once nothing more can be painted: a session that
        // touched many files must not build lines the height will drop.
        if lines.len() >= usize::from(height) {
            break;
        }
    }
    lines.truncate(usize::from(height));
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
}

/// Paint one full screen: transcript (or, when a sidebar route is active
/// and the terminal is too narrow for a side-by-side sidebar, that route's
/// content takes over the transcript area entirely — see
/// [`UiMode::shows_sidebar`]/`compute_layout`'s own narrow-terminal
/// collapse), composer, status line, and — when open — a modal overlay.
///
/// `transcript`/`viewport` are supplied by the caller (not owned by
/// `AppState`) because they are a rendering-local cache the caller
/// incrementally folds `AppState.transcript()` entries into across frames —
/// see `apps/rapid`'s own renderer for how that stays in sync. `viewport`
/// is taken by `&mut` only to resolve its current window; scrolling itself
/// is the caller's own responsibility (`TranscriptViewport::scroll_by`)
/// before calling this.
#[allow(clippy::too_many_arguments)]
pub fn paint_screen(
    state: &AppState,
    transcript: &Transcript,
    viewport: &TranscriptViewport,
    composer_lines: &[String],
    chrome: &StatusChrome,
    size: Rect,
    modal_open: bool,
    cancel: &CancellationToken,
) -> Screen {
    let composer_height = u16::try_from(composer_lines.len()).unwrap_or(u16::MAX);
    let layout = compute_screen_layout(state, size, composer_height, modal_open);
    let mut screen = Screen::blank(size.width(), size.height());

    let transcript_rect = layout.transcript();
    let window = viewport.visible(transcript);
    let transcript_lines: Vec<String> = window
        .rows()
        .iter()
        .map(|row| row.text().to_owned())
        .collect();

    let route = state.route();
    if layout.sidebar().is_empty() && route != UiRoute::Transcript {
        screen.paint_lines(
            transcript_rect,
            &sidebar_lines(
                route,
                state,
                transcript_rect.width(),
                transcript_rect.height(),
                cancel,
            ),
        );
    } else {
        screen.paint_lines(transcript_rect, &transcript_lines);
        if !layout.sidebar().is_empty() {
            screen.paint_lines(
                layout.sidebar(),
                &sidebar_lines(
                    route,
                    state,
                    layout.sidebar().width(),
                    layout.sidebar().height(),
                    cancel,
                ),
            );
        }
    }

    screen.paint_lines(layout.composer(), composer_lines);

    let status = render_status_with(state, chrome, layout.status().width());
    screen.paint_lines(layout.status(), &[status.text()]);

    if modal_open && !layout.modal().is_empty() {
        screen.paint_lines(
            layout.modal(),
            &modal_lines(state, layout.modal().width(), layout.modal().height()),
        );
    }

    screen
}

/// The modal: for a pending approval, what the human decides on — the
/// tool, the requesting surface's summary, the scope, and the body itself
/// (an edit's diff, a command's full script) expanded to fill the modal,
/// with a count of what did not fit — then how to answer. Everything shown
/// comes from the `approval.requested` record the projection folded (the
/// capability/risk/policy fields of `ApprovalViewModel` have no source in
/// the record, so they are not invented here).
fn modal_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut lines = Vec::new();
    for modal in state.modal_stack() {
        match modal {
            crate::state::Modal::Approval { id } => {
                let approval = state.approvals().get(id);
                if let Some(tool) = approval.and_then(|approval| approval.tool()) {
                    lines.push(format!("approval required: {tool}"));
                } else {
                    lines.push(format!("approval required: {id}"));
                }
                if let Some(summary) = approval.and_then(|approval| approval.summary()) {
                    lines.extend(summary.lines().map(str::to_owned));
                }
                if let Some(approval) = approval
                    && !approval.scope().is_empty()
                {
                    lines.push(format!("scope: {}", approval.scope().join(", ")));
                }
                let footer = "resolve with /approvals approve|deny <n>";
                if let Some(body) = approval.and_then(|approval| approval.body()) {
                    lines.push(String::new());
                    // Expanded into the room left above the footer; what
                    // does not fit is counted, never silently dropped.
                    let body: Vec<&str> = body.lines().collect();
                    let room = usize::from(height).saturating_sub(lines.len() + 1);
                    if body.len() <= room {
                        lines.extend(body.iter().map(|line| (*line).to_owned()));
                    } else if room > 0 {
                        let shown = room - 1;
                        lines.extend(body[..shown].iter().map(|line| (*line).to_owned()));
                        lines.push(format!(
                            "… {} more line(s); /approvals shows the whole ask",
                            body.len() - shown
                        ));
                    }
                }
                lines.push(footer.to_owned());
            }
            crate::state::Modal::ProtocolError => {
                lines.push("protocol error".to_owned());
                if let Some(reason) = state.protocol_error() {
                    lines.push(reason.to_owned());
                }
            }
        }
    }
    lines.truncate(usize::from(height));
    // Every modal line is untrusted text (a model-authored script or diff,
    // a summary, a scope): control characters are neutralized, as in every
    // other pane — an escape sequence or carriage return in a script must
    // not repaint the modal to show a different ask — and tabs become
    // spaces so a line cannot run past the modal's edge.
    for line in &mut lines {
        let safe = crate::sanitize::sanitize_untrusted(line);
        *line = fit_width(&expand_tabs(&safe), usize::from(width));
    }
    lines
}

/// `text` with each tab replaced by spaces to the next multiple of 4.
fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut col = 0usize;
    for ch in text.chars() {
        if ch == '\t' {
            let pad = 4 - col % 4;
            out.extend(std::iter::repeat_n(' ', pad));
            col += pad;
        } else {
            out.push(ch);
            col += 1;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        GoalLifecycle, GoalProjection, LocalUiEvent, TranscriptEntry, UiEvent, reduce,
    };
    use protocol::GoalId;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    /// One kernel event envelope, for tests that drive the real
    /// `reduce` fold. Hoisted out of `modal_open_shows_a_pending_approval_
    /// notice`, which defined it inline, so the denial tests can use the
    /// same construction rather than a second copy.
    fn kernel_event(
        seq: u64,
        kind: event_ledger::event::EventKind,
        payload: serde_json::Value,
    ) -> event_ledger::event::ErasedEventEnvelope {
        use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, RecordedAt};
        use protocol::{EventId, RedactionClass, SessionId, TraceId};

        let session: SessionId = "019c0000-0000-7000-8000-000000000010"
            .parse()
            .expect("session");
        let actor = ActorRef::new(ActorKind::System, "019c0000-0000-7000-8000-000000000016")
            .expect("actor");
        EventEnvelope::new(
            format!("019c0000-0000-7000-8000-{seq:012x}")
                .parse::<EventId>()
                .expect("event id"),
            session,
            seq,
            "2026-08-14T15:20:04.123Z"
                .parse::<RecordedAt>()
                .expect("recorded_at"),
            actor,
            TraceId::new(),
            kind,
            RedactionClass::Project,
            payload,
        )
    }

    fn routed(route: UiRoute, width: u16, height: u16) -> AppState {
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SetViewport { width, height }),
        );
        reduce(state, &UiEvent::Local(LocalUiEvent::SetRoute(route)))
    }

    fn full_transcript_screen(transcript: &Transcript, size: Rect) -> Screen {
        let state = routed(UiRoute::Transcript, size.width(), size.height());
        let viewport = TranscriptViewport::from_rect(state_layout_transcript_rect(&state, size));
        let mut viewport = viewport;
        viewport.follow_end();
        paint_screen(
            &state,
            transcript,
            &viewport,
            &[],
            &StatusChrome::default(),
            size,
            false,
            &cancel(),
        )
    }

    fn state_layout_transcript_rect(state: &AppState, size: Rect) -> Rect {
        compute_screen_layout(state, size, 1, false).transcript()
    }

    #[test]
    fn transcript_entries_render_with_distinct_glyphs_and_never_conflate_context_required_with_failure()
     {
        let mut transcript = Transcript::new();
        transcript.push_entry(&TranscriptEntry::User {
            text: "deploy the app".to_owned(),
        });
        transcript.push_entry(&TranscriptEntry::Assistant {
            text: "on it".to_owned(),
        });
        transcript.push_entry(&TranscriptEntry::ToolActivity {
            tool: "shell_exec".to_owned(),
            status: crate::state::ToolActivityStatus::Failed,
            detail: None,
        });
        transcript.push_entry(&TranscriptEntry::ToolActivity {
            tool: "ask_user".to_owned(),
            status: crate::state::ToolActivityStatus::ContextRequired,
            detail: None,
        });
        transcript.push_entry(&TranscriptEntry::TurnFailed {
            reason: "model_failed".to_owned(),
        });
        transcript.push_entry(&TranscriptEntry::TurnInterrupted);

        let screen = full_transcript_screen(&transcript, Rect::new(0, 0, 80, 24));
        let painted = screen.snapshot();

        assert!(painted.contains("> deploy the app"));
        assert!(painted.contains("on it"));
        assert!(painted.contains("✗ shell_exec"), "{painted}");
        assert!(painted.contains("❓ ask_user"), "{painted}");
        assert!(
            !painted.contains("✗ ask_user"),
            "context-required must never render with the failure glyph: {painted}"
        );
        assert!(painted.contains("(turn failed: model_failed)"));
        assert!(painted.contains("(interrupted)"));
    }

    #[test]
    fn switching_route_changes_sidebar_content_not_transcript_content() {
        let mut transcript = Transcript::new();
        transcript.push_entry(&TranscriptEntry::Assistant {
            text: "hello from the transcript".to_owned(),
        });
        let size = Rect::new(0, 0, 120, 24);

        let transcript_state = routed(UiRoute::Transcript, size.width(), size.height());
        let mut viewport = TranscriptViewport::from_rect(
            compute_screen_layout(&transcript_state, size, 1, false).transcript(),
        );
        viewport.follow_end();
        let transcript_screen = paint_screen(
            &transcript_state,
            &transcript,
            &viewport,
            &[],
            &StatusChrome::default(),
            size,
            false,
            &cancel(),
        );

        let goal_state_base = routed(UiRoute::Goals, size.width(), size.height());
        let goal_state = reduce(
            goal_state_base,
            &UiEvent::Local(LocalUiEvent::SyncGoal(GoalProjection::new(
                GoalId::new(),
                Some("ship the release".to_owned()),
                GoalLifecycle::Active,
                Some(10),
                Some(100_000),
                2,
                500,
            ))),
        );
        let mut goal_viewport = TranscriptViewport::from_rect(
            compute_screen_layout(&goal_state, size, 1, false).transcript(),
        );
        goal_viewport.follow_end();
        let goal_screen = paint_screen(
            &goal_state,
            &transcript,
            &goal_viewport,
            &[],
            &StatusChrome::default(),
            size,
            false,
            &cancel(),
        );

        assert!(
            transcript_screen
                .snapshot()
                .contains("hello from the transcript")
        );
        assert!(
            goal_screen.snapshot().contains("ship the release"),
            "{}",
            goal_screen.snapshot()
        );
        assert_ne!(
            transcript_screen.snapshot(),
            goal_screen.snapshot(),
            "switching route must actually change what gets painted"
        );
    }

    #[test]
    fn zero_size_screen_does_not_panic_and_stays_blank() {
        let transcript = Transcript::new();
        let size = Rect::new(0, 0, 0, 0);
        let state = routed(UiRoute::Transcript, 0, 0);
        let viewport = TranscriptViewport::from_rect(
            compute_screen_layout(&state, size, 1, false).transcript(),
        );
        let screen = paint_screen(
            &state,
            &transcript,
            &viewport,
            &[],
            &StatusChrome::default(),
            size,
            false,
            &cancel(),
        );
        assert_eq!(screen.width(), 0);
        assert_eq!(screen.height(), 0);
        assert!(screen.rows().is_empty());
    }

    #[test]
    fn tiny_terminal_does_not_panic_or_overflow() {
        let transcript = Transcript::new();
        for size in [
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 5, 3),
            Rect::new(0, 0, 1, 24),
            Rect::new(0, 0, 80, 1),
        ] {
            let state = routed(UiRoute::Agents, size.width(), size.height());
            let viewport = TranscriptViewport::from_rect(
                compute_screen_layout(&state, size, 1, false).transcript(),
            );
            let screen = paint_screen(
                &state,
                &transcript,
                &viewport,
                &[],
                &StatusChrome::default(),
                size,
                false,
                &cancel(),
            );
            assert_eq!(screen.width(), size.width());
            assert_eq!(screen.height(), size.height());
        }
    }

    #[test]
    fn a_denied_tool_call_shows_the_user_why_and_not_just_that_it_was_denied() {
        // The reason existed all along and went only to the model, as the
        // tool result: `TurnEvent::ToolDenied` carried `turn_id`, `call_id`
        // and `tool` and dropped the detail, so the transcript read
        // `⛔ workspace_write` — a refusal with no cause and no remedy, in
        // the one mode the product ships in by default.
        const REASON: &str = "workspace_write denied: requires approval; \
pre-approve it with `rapid permissions allow <tool>`";
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                event_ledger::event::EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                event_ledger::event::EventKind::ToolDenied,
                serde_json::json!({
                    "turn_id": "019c0000-0000-7000-8000-000000000012",
                    "call_id": "call-1",
                    "tool": "workspace_write",
                    "detail": REASON,
                }),
            )),
        );

        let entry = state
            .transcript()
            .iter()
            .find(|entry| {
                matches!(
                    entry,
                    crate::state::TranscriptEntry::ToolActivity {
                        status: crate::state::ToolActivityStatus::Denied,
                        ..
                    }
                )
            })
            .expect("a denial reaches the transcript");
        let (_, line) = crate::transcript::render_block_parts(entry);
        assert!(
            line.contains("workspace_write"),
            "the tool must still be named: {line}"
        );
        assert!(
            line.contains("rapid permissions allow"),
            "the reason, which names the remedy, must reach the user: {line}"
        );
    }

    #[test]
    fn a_tool_event_with_no_reason_renders_exactly_as_it_did_before() {
        // `detail` is optional and every other tool event omits it; adding
        // the field must not change how a completion or a plain failure
        // reads.
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                event_ledger::event::EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                event_ledger::event::EventKind::ToolCompleted,
                serde_json::json!({
                    "turn_id": "019c0000-0000-7000-8000-000000000012",
                    "call_id": "call-1",
                    "tool": "repo_read",
                }),
            )),
        );
        let entry = state
            .transcript()
            .iter()
            .find(|entry| matches!(entry, crate::state::TranscriptEntry::ToolActivity { .. }))
            .expect("entry");
        let (_, line) = crate::transcript::render_block_parts(entry);
        assert_eq!(line, "✓ repo_read");
    }

    #[test]
    fn the_context_panel_says_so_before_any_turn_has_compiled_one() {
        // The honest empty state: there is no context to describe until a
        // turn has compiled one, and inventing a zero-of-a-window would be a
        // figure the reader could act on wrongly.
        let painted = sidebar_lines(UiRoute::Context, &AppState::new(), 50, 6, &cancel());
        assert_eq!(
            painted,
            vec![
                fit_width("[context]  usage", 50),
                fit_width("no turn has compiled a context yet", 50)
            ]
        );
        assert!(route_renders_content(UiRoute::Context));
    }

    #[test]
    fn the_context_legend_sums_to_one_hundred() {
        for parts in [
            vec![1u64, 1, 1],
            vec![333, 333, 334, 0],
            vec![7, 0, 13, 80],
            vec![1, 2, 3, 4, 5, 6, 7],
            vec![0, 0],
            vec![u64::MAX / 4, 1, 1],
        ] {
            let shares = percent_shares(&parts);
            let total: u64 = shares.iter().sum();
            if parts.iter().all(|&part| part == 0) {
                assert_eq!(total, 0, "{parts:?}");
            } else {
                assert_eq!(total, 100, "{parts:?} -> {shares:?}");
            }
        }
        // Through the panel: every class and the free share, summing to 100.
        let state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                event_ledger::event::EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        let state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                event_ledger::event::EventKind::ContextCompiled,
                serde_json::json!({
                    "included_tokens": 700, "context_limit": 1000,
                    "partitions": [
                        {"class": "system", "used": 333, "cap": 400},
                        {"class": "user", "used": 367, "cap": 600}
                    ]
                }),
            )),
        );
        let painted = sidebar_lines(UiRoute::Context, &state, 60, 10, &cancel());
        let percent: u64 = painted
            .iter()
            .filter_map(|line| line.trim_end().strip_suffix('%'))
            .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
            .sum();
        assert_eq!(percent, 100, "{painted:?}");
        assert!(
            painted
                .iter()
                .any(|line| line.trim_end() == "  free 300 30%"),
            "{painted:?}"
        );
    }

    #[test]
    fn a_session_usage_stored_before_turns_were_kept_still_loads() {
        let stored = serde_json::json!({
            "turns": 2, "steps": 3, "tokens": 40, "input_tokens": null,
            "output_tokens": null, "cached_tokens": null, "tokens_estimated_steps": 3,
            "cost_known_usd_micros": 0, "cost_unknown_steps": 3, "last_turn": "t2"
        });
        let usage: crate::state::SessionUsage = serde_json::from_value(stored).expect("loads");
        assert_eq!((usage.turns, usage.steps, usage.tokens), (2, 3, 40));
    }

    #[test]
    fn the_usage_tab_folds_the_sessions_model_steps_and_never_calls_unknown_zero() {
        use event_ledger::event::EventKind;
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        for (seq, kind, payload) in [
            (
                2,
                EventKind::ModelCompleted,
                serde_json::json!({"turn_id": "t1", "tokens": 30, "input_tokens": 20,
                    "output_tokens": 10, "cached_tokens": 4, "tokens_estimated": false,
                    "cost": {"kind": "reported", "usd_micros": 1500}}),
            ),
            (
                3,
                EventKind::ModelCompleted,
                serde_json::json!({"turn_id": "t2", "tokens": 9, "input_tokens": 6,
                    "output_tokens": 3, "cached_tokens": 0, "tokens_estimated": false,
                    "cost": {"kind": "unknown"}}),
            ),
            // A failed step that recorded what it was billed for.
            (
                4,
                EventKind::ModelFailed,
                serde_json::json!({"turn_id": "t3", "tokens": null, "input_tokens": 2,
                    "output_tokens": 1, "cached_tokens": 0, "tokens_estimated": false,
                    "cost": {"kind": "reported", "usd_micros": 5}}),
            ),
            // A failure from before steps recorded usage: nothing to count.
            (
                5,
                EventKind::ModelFailed,
                serde_json::json!({"turn_id": "t4"}),
            ),
        ] {
            state = reduce(state, &UiEvent::Kernel(kernel_event(seq, kind, payload)));
        }
        let usage = state.session_usage();
        assert_eq!((usage.turns, usage.steps, usage.tokens), (3, 3, 42));
        assert_eq!(usage.input_tokens, Some(28));
        assert_eq!(usage.cost_usd_micros(), None);
        assert_eq!(usage.cost_known_usd_micros, 1505);
        state = reduce(
            state,
            &UiEvent::Local(crate::state::LocalUiEvent::SelectContextTab(
                crate::state::ContextTab::Usage,
            )),
        );
        let painted = sidebar_lines(UiRoute::Context, &state, 60, 12, &cancel());
        let text: Vec<&str> = painted.iter().map(|line| line.trim_end()).collect();
        assert_eq!(text[0], " context  [usage]");
        assert!(text.contains(&"cost unknown (>= $0.001505)"), "{text:?}");
        assert!(text.contains(&"basis unknown"), "{text:?}");
        assert!(text.contains(&"  input 28"), "{text:?}");
    }

    #[test]
    fn the_memory_panel_shows_the_index_and_neutralises_what_a_clone_can_carry() {
        // `MEMORY.md` is repository content: a clone can carry escape
        // sequences that move the cursor or repaint the frame. The panel is
        // a rendering of untrusted text and has to treat it as such.
        let empty = AppState::new();
        assert_eq!(
            sidebar_lines(UiRoute::Memory, &empty, 50, 6, &cancel()),
            vec![fit_width("no .rapidlm/MEMORY.md in this project", 50)],
            "a project with no memory index says so rather than painting nothing"
        );

        let state = reduce(
            empty,
            &UiEvent::Local(crate::state::LocalUiEvent::SyncMemory(vec![
                "- [Auth notes](auth.md) — where the tokens live".to_owned(),
                "\u{1b}[2J\u{1b}[Hcleared your screen".to_owned(),
            ])),
        );
        let painted = sidebar_lines(UiRoute::Memory, &state, 60, 6, &cancel());
        assert!(
            painted[0].contains("Auth notes"),
            "the index must actually be shown: {painted:?}"
        );
        assert!(
            !painted.iter().any(|line| line.contains('\u{1b}')),
            "no escape sequence from a cloned repository may reach the terminal: {painted:?}"
        );
        assert!(
            painted[1].contains("cleared your screen"),
            "and the text itself is still readable, just inert: {painted:?}"
        );
        assert!(route_renders_content(UiRoute::Memory));
    }

    #[test]
    fn the_models_panel_shows_which_model_runs_and_what_falls_back() {
        use crate::state::ModelRow;

        let empty = AppState::new();
        assert_eq!(
            sidebar_lines(UiRoute::Models, &empty, 40, 6, &cancel()),
            vec![fit_width("no models configured", 40)],
            "a project with no configured models says so rather than painting nothing"
        );

        let state = reduce(
            empty,
            &UiEvent::Local(crate::state::LocalUiEvent::SyncModels(vec![
                ModelRow {
                    id: "big".to_owned(),
                    provider: "anthropic".to_owned(),
                    model: "claude-opus-5".to_owned(),
                    active: true,
                    fallback_rank: None,
                    context_window: Some(200_000),
                },
                ModelRow {
                    id: "backup".to_owned(),
                    provider: "openai-compatible".to_owned(),
                    model: "gpt-5".to_owned(),
                    active: false,
                    fallback_rank: Some(0),
                    context_window: None,
                },
            ])),
        );
        let painted = sidebar_lines(UiRoute::Models, &state, 70, 6, &cancel());
        assert!(
            painted[0].starts_with("> big") && painted[0].contains("anthropic/claude-opus-5"),
            "the running model must be marked and named: {painted:?}"
        );
        assert!(
            painted[0].contains("ctx:200000"),
            "with its context window when configured: {painted:?}"
        );
        assert!(
            painted[1].starts_with("  backup") && painted[1].contains("fallback#1"),
            "and a fallback must show its position in the chain: {painted:?}"
        );
        assert!(route_renders_content(UiRoute::Models));
    }

    #[test]
    fn a_notification_is_a_notice_in_the_jobs_panel_never_the_transcript() {
        use event_ledger::event::EventKind;
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        let before = state.transcript().len();
        for seq in 2..(2 + crate::state::MAX_NOTIFICATIONS as u64 + 3) {
            state = reduce(
                state,
                &UiEvent::Kernel(kernel_event(
                    seq,
                    EventKind::NotificationRecorded,
                    serde_json::json!({"source": "loop cron-1", "text": format!("build ok {seq}")}),
                )),
            );
        }
        assert_eq!(
            state.transcript().len(),
            before,
            "never in the conversation"
        );
        assert_eq!(state.notifications().len(), crate::state::MAX_NOTIFICATIONS);
        let painted = sidebar_lines(UiRoute::Jobs, &state, 60, 8, &cancel());
        assert!(
            painted.iter().any(|line| line.contains("notifications")),
            "{painted:?}"
        );
        let last = 2 + crate::state::MAX_NOTIFICATIONS as u64 + 2;
        assert!(
            painted
                .iter()
                .any(|line| line.contains(&format!("loop cron-1: build ok {last}"))),
            "newest first: {painted:?}"
        );
    }

    #[test]
    fn a_hostile_notice_neither_freezes_the_session_nor_drives_the_terminal() {
        use event_ledger::event::EventKind;
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        let notices = [
            serde_json::json!({"source": "loop a", "text": "x".repeat(17 * 1024)}),
            serde_json::json!({"source": 5, "text": ["not", "a", "string"]}),
            serde_json::json!({"source": "loop\u{1b}[2J", "text": "\u{1b}]0;pwned\u{7}hi\rX"}),
        ];
        for (offset, payload) in notices.into_iter().enumerate() {
            state = reduce(
                state,
                &UiEvent::Kernel(kernel_event(
                    2 + offset as u64,
                    EventKind::NotificationRecorded,
                    payload,
                )),
            );
        }
        assert!(!state.actions_blocked(), "no protocol error");
        assert_eq!(state.notifications().len(), 3, "each one kept");
        assert!(state.notifications()[0].text().len() <= crate::state::MAX_NOTIFICATION_TEXT_BYTES);
        assert_eq!(state.notifications()[1].source(), "notification");
        // The session goes on: a later job still lands.
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                5,
                EventKind::JobStarted,
                serde_json::json!({"job_id": "019c0000-0000-7000-8000-00000000002a"}),
            )),
        );
        assert_eq!(state.jobs().len(), 1);
        let painted = sidebar_lines(UiRoute::Jobs, &state, 80, 12, &cancel()).join("\n");
        assert!(!painted.contains('\u{1b}'), "{painted:?}");
        assert!(!painted.contains('\u{7}'), "{painted:?}");
        assert!(!painted.contains('\r'), "{painted:?}");
    }

    #[test]
    fn notices_are_not_what_a_full_jobs_panel_cuts() {
        use event_ledger::event::EventKind;
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        for n in 0..10u64 {
            state = reduce(
                state,
                &UiEvent::Kernel(kernel_event(
                    2 + n,
                    EventKind::JobStarted,
                    serde_json::json!({"job_id": format!("019c0000-0000-7000-8000-0000000000{:02x}", 0x30 + n)}),
                )),
            );
        }
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                12,
                EventKind::NotificationRecorded,
                serde_json::json!({"source": "loop a", "text": "build green"}),
            )),
        );
        let painted = sidebar_lines(UiRoute::Jobs, &state, 60, 6, &cancel());
        assert_eq!(painted.len(), 6);
        assert!(
            painted.iter().any(|line| line.contains("build green")),
            "{painted:?}"
        );
    }

    #[test]
    fn the_agents_panel_lists_the_agent_types_under_the_agents() {
        use crate::state::{AgentTypeRow, LocalUiEvent};
        let rows = |n: usize| {
            (0..n)
                .map(|i| AgentTypeRow {
                    id: format!("type-{i}"),
                    line: format!("type-{i}  explorer  project  looks\u{1b}[2J"),
                })
                .collect::<Vec<_>>()
        };
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SyncAgentTypes(rows(2))),
        );
        let painted = sidebar_lines(UiRoute::Agents, &state, 60, 12, &cancel());
        let at = painted
            .iter()
            .position(|line| line.starts_with("agent types (task_spawn type=<id>)"))
            .unwrap_or_else(|| panic!("no types section: {painted:?}"));
        assert!(painted[at + 1].starts_with("  type-0  explorer  project  looks"));
        assert!(
            !painted.iter().any(|line| line.contains('\u{1b}')),
            "sanitised"
        );
        // Many types, short panel: cut with a count, never a bare header.
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SyncAgentTypes(rows(30))),
        );
        let painted = sidebar_lines(UiRoute::Agents, &state, 60, 6, &cancel());
        assert!(painted.len() <= 6, "{painted:?}");
        assert!(
            painted
                .last()
                .is_some_and(|line| line.contains("… 26 more (rapid agents list)")),
            "{painted:?}"
        );
    }

    #[test]
    fn a_short_panel_cuts_loops_with_a_count_never_a_bare_header() {
        use crate::state::{LocalUiEvent, LoopRow};
        use event_ledger::event::EventKind;
        let rows = (0..8)
            .map(|n| LoopRow {
                id: format!("cron-{n}"),
                line: format!("cron-{n}  every 5m"),
            })
            .collect();
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        state = reduce(state, &UiEvent::Local(LocalUiEvent::SyncLoops(rows)));
        let painted = sidebar_lines(UiRoute::Jobs, &state, 60, 5, &cancel());
        assert_eq!(painted.len(), 5, "{painted:?}");
        assert!(painted[0].starts_with("loops"), "{painted:?}");
        assert!(painted[4].contains("… 5 more"), "{painted:?}");
        // With a notice and one row left for loops: a count, not a header.
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                EventKind::NotificationRecorded,
                serde_json::json!({"source": "loop a", "text": "green"}),
            )),
        );
        let painted = sidebar_lines(UiRoute::Jobs, &state, 60, 3, &cancel());
        assert_eq!(
            painted
                .iter()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>(),
            vec![
                "loops: 8 (/loop lists them)",
                "notifications",
                "  loop a: green"
            ]
        );
    }

    #[test]
    fn the_jobs_panel_lists_the_sessions_loops() {
        use crate::state::{LocalUiEvent, LoopRow};
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SyncLoops(vec![LoopRow {
                id: "cron-1".to_owned(),
                line: "cron-1  every */5 * * * *  next in 4m  expires in 6d  check \u{1b}[2Jci"
                    .to_owned(),
            }])),
        );
        let painted = sidebar_lines(UiRoute::Jobs, &state, 80, 8, &cancel());
        assert!(
            painted.iter().any(|line| line.starts_with("loops")),
            "{painted:?}"
        );
        let row = painted
            .iter()
            .find(|line| line.contains("cron-1"))
            .expect("the loop row");
        assert!(
            row.contains("next in 4m") && row.contains("expires in 6d"),
            "{row}"
        );
        assert!(!row.contains('\u{1b}'), "sanitised: {row:?}");
    }

    #[test]
    fn the_jobs_panel_shows_the_jobs_the_projection_already_had() {
        // `reduce` has populated `AppState::jobs` from `job.*` events all
        // along; the route simply painted nothing, so `/jobs` opened an empty
        // panel — which on a narrow terminal takes the whole transcript rect.
        use event_ledger::event::EventKind;

        let job = "019c0000-0000-7000-8000-00000000002a";
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        assert_eq!(
            sidebar_lines(UiRoute::Jobs, &state, 40, 6, &cancel()),
            vec![fit_width("no jobs", 40)],
            "an empty projection says so rather than painting nothing at all"
        );

        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                EventKind::JobStarted,
                serde_json::json!({"job_id": job}),
            )),
        );
        let started = sidebar_lines(UiRoute::Jobs, &state, 60, 6, &cancel());
        // The id's random tail, not the full UUID: it is the part that
        // distinguishes UUIDv7s minted close together, the same convention
        // the agents panel uses, and what `IdResolver` accepts back.
        assert!(
            started[0].contains("00000000002a") && started[0].contains("started"),
            "the job and its state must both be shown: {started:?}"
        );

        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                3,
                EventKind::JobCompleted,
                serde_json::json!({"job_id": job, "exit_status": 3}),
            )),
        );
        let done = sidebar_lines(UiRoute::Jobs, &state, 60, 6, &cancel());
        assert!(
            done[0].contains("completed") && done[0].contains("exit:3"),
            "a finished job must show how it finished: {done:?}"
        );
        assert!(
            route_renders_content(UiRoute::Jobs),
            "and the route must now report itself as one that paints"
        );
    }

    #[test]
    fn context_search_results_cannot_repaint_the_terminal() {
        // Locators are repository paths — a clone controls them, filename
        // included — so they are untrusted strings like every other one the
        // compositor paints.
        use crate::state::{ContextHit, ContextSearchOutcome, ContextSearchView};

        let state = reduce(
            AppState::new(),
            &UiEvent::Local(crate::state::LocalUiEvent::SyncContextSearch(Some(
                ContextSearchView::new(
                    "eviction".to_owned(),
                    vec![ContextHit {
                        locator: "\u{1b}[2J\u{1b}[Hsrc/lru.py".to_owned(),
                        bytes: 75,
                    }],
                    ContextSearchOutcome::Searched,
                ),
            ))),
        );
        let painted = sidebar_lines(UiRoute::Context, &state, 60, 8, &cancel());
        assert!(
            !painted.iter().any(|line| line.contains('\u{1b}')),
            "no escape sequence from a repository path may reach the terminal: {painted:?}"
        );
        assert!(
            painted.iter().any(|line| line.contains("src/lru.py")),
            "and the path is still readable: {painted:?}"
        );

        // A query that retrieved nothing says so, rather than painting a
        // bare header that looks like a rendering bug.
        let empty = reduce(
            AppState::new(),
            &UiEvent::Local(crate::state::LocalUiEvent::SyncContextSearch(Some(
                ContextSearchView::new(
                    "nothing matches this".to_owned(),
                    Vec::new(),
                    ContextSearchOutcome::Searched,
                ),
            ))),
        );
        let painted = sidebar_lines(UiRoute::Context, &empty, 60, 8, &cancel());
        assert!(
            painted
                .iter()
                .any(|line| line.contains("nothing retrieved")),
            "an empty result must say so: {painted:?}"
        );
    }

    #[test]
    fn the_types_count_is_every_type_synced_not_those_kept() {
        use crate::state::{AgentTypeRow, LocalUiEvent, MAX_AGENT_TYPE_ROWS};
        let rows: Vec<AgentTypeRow> = (0..134)
            .map(|n| AgentTypeRow {
                id: format!("t{n}"),
                line: format!("t{n}  explorer  project  looks"),
            })
            .collect();
        let state = reduce(
            AppState::new(),
            &UiEvent::Local(LocalUiEvent::SyncAgentTypes(rows)),
        );
        assert_eq!(state.agent_types().len(), MAX_AGENT_TYPE_ROWS);
        assert_eq!(state.agent_types_total(), 134);
        let painted = sidebar_lines(UiRoute::Agents, &state, 60, 20, &cancel());
        // A header, 18 rows, and the count of the other 116.
        assert_eq!(painted.len(), 20, "{painted:?}");
        assert_eq!(
            painted.last().map(String::as_str),
            Some("  … 116 more (rapid agents list)")
        );
    }

    #[test]
    fn the_agent_types_never_cut_an_agents_detail() {
        use crate::state::{AgentTypeRow, LocalUiEvent};
        use event_ledger::event::EventKind;
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        for (seq, id) in [
            (2u64, "019c0000-0000-7000-8000-0000000000a1"),
            (3, "019c0000-0000-7000-8000-0000000000a2"),
        ] {
            state = reduce(
                state,
                &UiEvent::Kernel(kernel_event(
                    seq,
                    EventKind::AgentSpawned,
                    serde_json::json!({"agent_id": id, "role": "planner", "state": "running"}),
                )),
            );
        }
        let without = sidebar_lines(UiRoute::Agents, &state, 70, 40, &cancel());
        let state = reduce(
            state,
            &UiEvent::Local(LocalUiEvent::SyncAgentTypes(
                (0..4)
                    .map(|n| AgentTypeRow {
                        id: format!("t{n}"),
                        line: format!("t{n}  explorer  builtin  looks"),
                    })
                    .collect(),
            )),
        );
        for height in [4u16, 12, 40] {
            let with = sidebar_lines(UiRoute::Agents, &state, 70, height, &cancel());
            let agents: Vec<&String> = without.iter().take(usize::from(height)).collect();
            assert_eq!(
                with.iter().take(agents.len()).collect::<Vec<_>>(),
                agents,
                "height {height}: the agents' lines, whole"
            );
            assert!(with.len() <= usize::from(height), "{with:?}");
        }
        let tall = sidebar_lines(UiRoute::Agents, &state, 70, 40, &cancel());
        assert!(
            tall.iter().any(|line| line.starts_with("agent types")),
            "{tall:?}"
        );
    }

    #[test]
    fn the_agents_panel_describes_the_agent_that_was_selected() {
        // Why `/agents show <id>` setting `selected_agent` matters: the
        // panel resolves that field into its selected row and paints both a
        // `>` marker and that row's detail block. With the field unset it
        // falls back to row 0, so before the command wrote it the panel
        // described whichever agent sorted first no matter which one was
        // named.
        use event_ledger::event::EventKind;

        let first = "019c0000-0000-7000-8000-0000000000a1";
        let second = "019c0000-0000-7000-8000-0000000000a2";
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        for (seq, id, role) in [(2u64, first, "planner"), (3, second, "reviewer")] {
            state = reduce(
                state,
                &UiEvent::Kernel(kernel_event(
                    seq,
                    EventKind::AgentSpawned,
                    serde_json::json!({"agent_id": id, "role": role, "state": "running"}),
                )),
            );
        }

        // With nothing selected the panel falls back to row 0 and describes
        // it — which is exactly why a dropped `/agents show <id>` was worse
        // than inert: it confidently detailed the wrong agent.
        let unselected = sidebar_lines(UiRoute::Agents, &state, 70, 12, &cancel());
        assert!(
            unselected
                .iter()
                .any(|line| line.contains("selected:0000000000a1")),
            "with nothing selected the panel describes the first row: {unselected:?}"
        );

        let chosen: protocol::AgentId = second.parse().expect("agent id");
        let state = reduce(
            state,
            &UiEvent::Local(crate::state::LocalUiEvent::SelectAgent(Some(chosen))),
        );
        let painted = sidebar_lines(UiRoute::Agents, &state, 70, 12, &cancel());
        assert!(
            painted
                .iter()
                .any(|line| line.contains("selected:0000000000a2")),
            "the detail block must describe the agent that was named: {painted:?}"
        );
        assert!(
            !painted
                .iter()
                .any(|line| line.contains("selected:0000000000a1")),
            "and not the one that merely sorted first: {painted:?}"
        );
        let marker_row = painted
            .iter()
            .find(|line| line.trim_start().starts_with('>'))
            .expect("some row carries the selection marker");
        assert!(
            marker_row.contains("0000000000a2"),
            "and the row marker moves with it: {painted:?}"
        );
    }

    #[test]
    fn diff_hunks_are_file_content_and_cannot_repaint_the_terminal() {
        // A hunk line is a line of a source file the agent wrote — the most
        // direct route for an escape sequence to reach the terminal. And a
        // write that carried no hunks (binary, or past the differ's bounds)
        // shows its counts alone rather than an invented diff.
        use event_ledger::event::EventKind;

        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                EventKind::WorkspaceMutationDetected,
                serde_json::json!({
                    "path": "src/main.rs",
                    "lines_before": 3,
                    "lines_after": 3,
                    "hunks": "@@ -1,3 +1,3 @@\n fn a() {}\n-fn b() {}\n+fn b() { \u{1b}[2J }\n fn c() {}\n",
                }),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                3,
                EventKind::WorkspaceMutationDetected,
                serde_json::json!({
                    "path": "assets/logo.png",
                    "lines_before": 40,
                    "lines_after": 41,
                }),
            )),
        );
        let painted = sidebar_lines(UiRoute::Diff, &state, 70, 12, &cancel());
        assert!(
            !painted.iter().any(|line| line.contains('\u{1b}')),
            "no escape sequence from a written file may reach the terminal: {painted:?}"
        );
        assert!(
            painted.iter().any(|line| line.contains("+fn b() {")),
            "and the hunk is still readable: {painted:?}"
        );
        let logo = painted
            .iter()
            .position(|line| line.contains("assets/logo.png"))
            .expect("the file without hunks still has its row");
        assert!(
            painted[logo].contains("40 -> 41 lines"),
            "counts alone for a write that carried no hunks: {painted:?}"
        );
        assert!(
            painted
                .get(logo + 1)
                .is_none_or(|next| !next.starts_with("  @@")),
            "and no hunk is invented under it: {painted:?}"
        );
    }

    #[test]
    fn a_logs_page_never_paints_under_a_different_jobs_selection() {
        // The page carries the job it was synced for precisely so this
        // cannot happen: a user who runs `/jobs logs A` and then `/jobs
        // show B` must not see A's output labelled as B. Without the tag
        // the panel would paint whatever page was last synced.
        use event_ledger::event::EventKind;

        let a = "019c0000-0000-7000-8000-00000000002a";
        let b = "019c0000-0000-7000-8000-00000000002b";
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        for (seq, id) in [(2u64, a), (3, b)] {
            state = reduce(
                state,
                &UiEvent::Kernel(kernel_event(
                    seq,
                    EventKind::JobStarted,
                    serde_json::json!({"job_id": id}),
                )),
            );
        }

        let a_id: JobId = a.parse().expect("job id");
        let b_id: JobId = b.parse().expect("job id");
        let state = reduce(
            state,
            &UiEvent::Local(crate::state::LocalUiEvent::SyncJobLogs(Some(
                crate::state::JobLogView::new(a_id, vec!["output-of-a".to_owned()], false),
            ))),
        );

        let selected_a = reduce(
            state.clone(),
            &UiEvent::Local(crate::state::LocalUiEvent::SelectJob(Some(a_id))),
        );
        let painted = sidebar_lines(UiRoute::Jobs, &selected_a, 60, 6, &cancel());
        assert!(
            painted.iter().any(|line| line.contains("output-of-a")),
            "the page must paint for the job it belongs to: {painted:?}"
        );

        let selected_b = reduce(
            state,
            &UiEvent::Local(crate::state::LocalUiEvent::SelectJob(Some(b_id))),
        );
        let painted = sidebar_lines(UiRoute::Jobs, &selected_b, 60, 6, &cancel());
        assert!(
            !painted.iter().any(|line| line.contains("output-of-a")),
            "one job's output must never be painted under another's selection: {painted:?}"
        );
        assert!(
            painted.iter().any(|line| line.contains(b)),
            "and the job that was named is the one shown: {painted:?}"
        );
    }

    #[test]
    fn job_output_cannot_repaint_the_terminal_around_it() {
        // Log text is whatever a child process wrote to its own stdout —
        // the most obviously attacker-influenced string the compositor
        // paints (a build log echoes filenames, test names, and remote
        // content). It goes through `sanitize_untrusted` like every other
        // untrusted string.
        use event_ledger::event::EventKind;

        let job = "019c0000-0000-7000-8000-00000000002a";
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                EventKind::JobStarted,
                serde_json::json!({"job_id": job}),
            )),
        );
        let id: JobId = job.parse().expect("job id");
        state = reduce(
            state,
            &UiEvent::Local(crate::state::LocalUiEvent::SelectJob(Some(id))),
        );
        state = reduce(
            state,
            &UiEvent::Local(crate::state::LocalUiEvent::SyncJobLogs(Some(
                crate::state::JobLogView::new(
                    id,
                    vec!["\u{1b}[2J\u{1b}[Hcompiling the payload".to_owned()],
                    false,
                ),
            ))),
        );
        let painted = sidebar_lines(UiRoute::Jobs, &state, 60, 6, &cancel());
        assert!(
            !painted.iter().any(|line| line.contains('\u{1b}')),
            "no escape sequence from a job's stdout may reach the terminal: {painted:?}"
        );
        assert!(
            painted
                .iter()
                .any(|line| line.contains("compiling the payload")),
            "and the text is still readable, just inert: {painted:?}"
        );
    }

    #[test]
    fn a_job_that_timed_out_or_was_cancelled_does_not_freeze_the_session() {
        // `reduce` answers an unparseable field by blocking every later
        // action and raising a protocol-error modal. `JobLifecycle::parse`
        // knew four states while `process_supervisor::JobState` speaks seven,
        // so a background job merely *timing out* was enough to freeze the
        // session the moment anything actually reported one.
        use crate::state::JobLifecycle;
        use event_ledger::event::EventKind;

        for (state, expected) in [
            ("completed", JobLifecycle::Completed),
            ("failed", JobLifecycle::Failed),
            ("cancelled", JobLifecycle::Cancelled),
            ("timed_out", JobLifecycle::TimedOut),
            ("orphaned", JobLifecycle::OrphanReconciled),
            ("running", JobLifecycle::Started),
        ] {
            let job = "019c0000-0000-7000-8000-00000000002a";
            let mut ui = reduce(
                AppState::new(),
                &UiEvent::Kernel(kernel_event(
                    1,
                    EventKind::SessionCreated,
                    serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
                )),
            );
            ui = reduce(
                ui,
                &UiEvent::Kernel(kernel_event(
                    2,
                    EventKind::JobCompleted,
                    serde_json::json!({"job_id": job, "state": state}),
                )),
            );
            assert!(
                !ui.actions_blocked(),
                "a job reported as {state:?} must not block the session: {:?}",
                ui.protocol_error()
            );
            let projected = ui.jobs().values().next().expect("the job is projected");
            assert_eq!(projected.state(), expected, "state {state:?}");
        }
    }

    #[test]
    fn the_approvals_panel_distinguishes_granted_from_refused() {
        // "resolved" alone cannot tell an approval that was granted from one
        // that was refused, which is the one fact this panel exists for.
        use event_ledger::event::EventKind;

        let approval = "019c0000-0000-7000-8000-00000000001a";
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(kernel_event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        assert_eq!(
            sidebar_lines(UiRoute::Approvals, &state, 40, 6, &cancel()),
            vec![fit_width("no approvals", 40)]
        );

        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                2,
                EventKind::ApprovalRequested,
                serde_json::json!({"approval_id": approval}),
            )),
        );
        let pending = sidebar_lines(UiRoute::Approvals, &state, 60, 6, &cancel());
        assert!(
            pending[0].contains(approval) && pending[0].contains("requested"),
            "a pending approval must be listed: {pending:?}"
        );

        state = reduce(
            state,
            &UiEvent::Kernel(kernel_event(
                3,
                EventKind::ApprovalResolved,
                serde_json::json!({"approval_id": approval, "decision": "denied"}),
            )),
        );
        let resolved = sidebar_lines(UiRoute::Approvals, &state, 60, 6, &cancel());
        assert!(
            resolved[0].contains("denied"),
            "and a resolved one must say which way it went: {resolved:?}"
        );
        assert!(route_renders_content(UiRoute::Approvals));
    }

    /// A state with one pending approval whose `approval.requested` record
    /// carries `payload`, its modal open.
    fn state_with_pending_approval(payload: serde_json::Value) -> AppState {
        use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
        use protocol::{EventId, RedactionClass, SessionId, TraceId};
        let session: SessionId = "019c0000-0000-7000-8000-000000000010"
            .parse()
            .expect("session");
        let actor = ActorRef::new(ActorKind::System, "019c0000-0000-7000-8000-000000000016")
            .expect("actor");
        let event = |seq: u64, kind: EventKind, payload: serde_json::Value| {
            EventEnvelope::new(
                format!("019c0000-0000-7000-8000-{seq:012x}")
                    .parse::<EventId>()
                    .expect("event id"),
                session,
                seq,
                "2026-08-14T15:20:04.123Z"
                    .parse::<RecordedAt>()
                    .expect("recorded_at"),
                actor.clone(),
                TraceId::new(),
                kind,
                RedactionClass::Project,
                payload,
            )
        };
        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        let tool = payload["tool"].clone();
        state = reduce(
            state,
            &UiEvent::Kernel(event(2, EventKind::ApprovalRequested, payload)),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(event(
                3,
                EventKind::ToolApprovalRequired,
                serde_json::json!({
                    "turn_id": "019c0000-0000-7000-8000-000000000012",
                    "call_id": "call-1",
                    "tool": tool,
                    "id": "019c0000-0000-7000-8000-00000000001a",
                }),
            )),
        );
        state
    }

    fn modal_snapshot(state: &AppState, size: Rect) -> Vec<String> {
        let viewport =
            TranscriptViewport::from_rect(compute_screen_layout(state, size, 1, true).transcript());
        let screen = paint_screen(
            state,
            &Transcript::new(),
            &viewport,
            &[],
            &StatusChrome::default(),
            size,
            true,
            &cancel(),
        );
        let modal = compute_screen_layout(state, size, 1, true).modal();
        screen
            .snapshot()
            .lines()
            .skip(usize::from(modal.y()))
            .take(usize::from(modal.height()))
            .map(|line| {
                line.chars()
                    .skip(usize::from(modal.x()))
                    .take(usize::from(modal.width()))
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_approval_modal_shows_the_full_script_and_the_diff() {
        // Replaces the three-line modal (tool, summary, how to answer): the
        // human decides on what will run — the whole script — and on what
        // an edit changes — the whole diff — not on a one-line summary.
        let script = serde_json::json!({
            "id": "019c0000-0000-7000-8000-00000000001a",
            "call_id": "call-1",
            "tool": "shell_exec",
            "summary": "run: bash -c set -e ...",
            "scope": ["."],
            "diff": "$ bash -c <script below>\nset -e\ncargo build\ncargo test -- --nocapture",
        });
        assert_eq!(
            modal_snapshot(
                &state_with_pending_approval(script),
                Rect::new(0, 0, 80, 24)
            )[..9],
            [
                "approval required: shell_exec",
                "run: bash -c set -e ...",
                "scope: .",
                "",
                "$ bash -c <script below>",
                "set -e",
                "cargo build",
                "cargo test -- --nocapture",
                "resolve with /approvals approve|deny <n>",
            ]
        );
        let diff = serde_json::json!({
            "id": "019c0000-0000-7000-8000-00000000001a",
            "call_id": "call-1",
            "tool": "workspace_patch",
            "summary": "patch src/lib.rs",
            "scope": ["src/lib.rs"],
            "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new() {}",
        });
        assert_eq!(
            modal_snapshot(&state_with_pending_approval(diff), Rect::new(0, 0, 80, 24))[..10],
            [
                "approval required: workspace_patch",
                "patch src/lib.rs",
                "scope: src/lib.rs",
                "",
                "--- a/src/lib.rs",
                "+++ b/src/lib.rs",
                "@@ -1 +1 @@",
                "-fn old() {}",
                "+fn new() {}",
                "resolve with /approvals approve|deny <n>",
            ]
        );
    }

    #[test]
    fn a_script_cannot_repaint_the_modal_with_control_characters() {
        let hostile = serde_json::json!({
            "id": "019c0000-0000-7000-8000-00000000001a",
            "call_id": "call-1",
            "tool": "shell_exec",
            "summary": "run: bash -c ls\r\u{1b}[2Jrun: git status",
            "scope": ["."],
            "diff": "$ bash -c <script below>\nls\n\u{1b}[2J\u{1b}[Happroval required: shell_exec\rrun: git status\n\tindented\u{202e}",
        });
        let lines = modal_snapshot(
            &state_with_pending_approval(hostile),
            Rect::new(0, 0, 80, 24),
        );
        for line in &lines {
            assert!(
                !line
                    .chars()
                    .any(|c| c == '\u{1b}' || c == '\r' || c == '\u{202e}' || c == '\t'),
                "a control character reached the modal: {line:?}"
            );
        }
        // Still exactly one header: the script's fake one is text in the
        // body, not a repainted modal.
        assert_eq!(lines[0], "approval required: shell_exec");
        assert!(
            lines.iter().any(|line| line.starts_with("    indented")),
            "{lines:#?}"
        );
    }

    #[test]
    fn a_body_longer_than_the_modal_is_counted_and_the_answer_stays_visible() {
        let body: Vec<String> = (1..=60).map(|n| format!("+line {n}")).collect();
        let long = serde_json::json!({
            "id": "019c0000-0000-7000-8000-00000000001a",
            "call_id": "call-1",
            "tool": "workspace_write",
            "summary": "create big.txt",
            "scope": ["big.txt"],
            "diff": body.join("\n"),
        });
        let lines = modal_snapshot(&state_with_pending_approval(long), Rect::new(0, 0, 80, 24));
        let shown: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with("+line"))
            .collect();
        assert!(!shown.is_empty());
        let rest = 60 - shown.len();
        assert!(
            lines
                .iter()
                .any(|line| line
                    == &format!("… {rest} more line(s); /approvals shows the whole ask")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "resolve with /approvals approve|deny <n>"),
            "{lines:#?}"
        );
    }

    #[test]
    fn modal_open_shows_a_pending_approval_notice() {
        use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
        use protocol::{EventId, RedactionClass, SessionId, TraceId};

        let session: SessionId = "019c0000-0000-7000-8000-000000000010"
            .parse()
            .expect("session");
        let actor = ActorRef::new(ActorKind::System, "019c0000-0000-7000-8000-000000000016")
            .expect("actor");
        let event = |seq: u64, kind: EventKind, payload: serde_json::Value| {
            EventEnvelope::new(
                format!("019c0000-0000-7000-8000-{seq:012x}")
                    .parse::<EventId>()
                    .expect("event id"),
                session,
                seq,
                "2026-08-14T15:20:04.123Z"
                    .parse::<RecordedAt>()
                    .expect("recorded_at"),
                actor.clone(),
                TraceId::new(),
                kind,
                RedactionClass::Project,
                payload,
            )
        };

        let mut state = reduce(
            AppState::new(),
            &UiEvent::Kernel(event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            )),
        );
        // The durable request opens the modal (with the action summary the
        // requesting surface recorded); the in-flight tool event adds the
        // transcript glyph. Both are emitted in production, in this order.
        state = reduce(
            state,
            &UiEvent::Kernel(event(
                2,
                EventKind::ApprovalRequested,
                serde_json::json!({
                    "id": "019c0000-0000-7000-8000-00000000001a",
                    "call_id": "call-1",
                    "tool": "shell_exec",
                    "summary": "run: cargo test",
                }),
            )),
        );
        state = reduce(
            state,
            &UiEvent::Kernel(event(
                3,
                EventKind::ToolApprovalRequired,
                serde_json::json!({
                    "turn_id": "019c0000-0000-7000-8000-000000000012",
                    "call_id": "call-1",
                    "tool": "shell_exec",
                    "id": "019c0000-0000-7000-8000-00000000001a",
                }),
            )),
        );
        let size = Rect::new(0, 0, 80, 24);
        let viewport = TranscriptViewport::from_rect(
            compute_screen_layout(&state, size, 1, true).transcript(),
        );
        let screen = paint_screen(
            &state,
            &Transcript::new(),
            &viewport,
            &[],
            &StatusChrome::default(),
            size,
            true,
            &cancel(),
        );
        // The modal leads with the action (the tool the pending call names)
        // now that the projection carries it — what the user needs to decide
        // — and falls back to the id when the event named no tool.
        assert!(
            screen.snapshot().contains("approval required: shell_exec"),
            "{}",
            screen.snapshot()
        );
    }

    #[test]
    fn modal_without_a_named_tool_falls_back_to_the_approval_id() {
        use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
        use protocol::{EventId, RedactionClass, SessionId, TraceId};

        let session: SessionId = "019c0000-0000-7000-8000-000000000010"
            .parse()
            .expect("session");
        let actor = ActorRef::new(ActorKind::System, "019c0000-0000-7000-8000-000000000016")
            .expect("actor");
        let event = |seq: u64, kind: EventKind, payload: serde_json::Value| {
            UiEvent::Kernel(EventEnvelope::new(
                format!("019c0000-0000-7000-8000-{seq:012x}")
                    .parse::<EventId>()
                    .expect("event id"),
                session,
                seq,
                "2026-08-14T15:20:04.123Z"
                    .parse::<RecordedAt>()
                    .expect("recorded_at"),
                actor.clone(),
                TraceId::new(),
                kind,
                RedactionClass::Project,
                payload,
            ))
        };

        let state = reduce(
            AppState::new(),
            &event(
                1,
                EventKind::SessionCreated,
                serde_json::json!({"project_id": "019c0000-0000-7000-8000-000000000011"}),
            ),
        );
        let state = reduce(
            state,
            &event(
                2,
                EventKind::ApprovalRequested,
                serde_json::json!({ "approval_id": "019c0000-0000-7000-8000-00000000001a" }),
            ),
        );
        let cancel = cancel();
        let size = Rect::new(0, 0, 80, 24);
        let viewport = TranscriptViewport::from_rect(
            compute_screen_layout(&state, size, 1, true).transcript(),
        );
        let screen = paint_screen(
            &state,
            &Transcript::new(),
            &viewport,
            &[],
            &StatusChrome::default(),
            size,
            true,
            &cancel,
        );
        assert!(
            screen
                .snapshot()
                .contains("approval required: 019c0000-0000-7000-8000-00000000001a"),
            "{}",
            screen.snapshot()
        );
    }
}
