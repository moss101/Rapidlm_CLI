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

use crate::layout::{LayoutRects, Rect, UiMode, compute_layout_with_composer};
use crate::panels::agents::{AgentsSelection, AgentsViewModel};
use crate::panels::goals::GoalViewModel;
use crate::state::{AppState, CancellationToken, UiRoute};
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
        UiRoute::Agents => AgentsViewModel::from_state(state, &[], AgentsSelection::default(), cancel)
            .map(|model| model.render(width, height).lines().to_vec())
            .unwrap_or_default(),
        UiRoute::Goals => goal_lines(state, width, height),
        UiRoute::Diff
        | UiRoute::Context
        | UiRoute::Memory
        | UiRoute::Jobs
        | UiRoute::Approvals
        | UiRoute::Graph
        | UiRoute::Computer
        | UiRoute::Resources
        | UiRoute::Models => Vec::new(),
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
    let transcript_lines: Vec<String> = window.rows().iter().map(|row| row.text().to_owned()).collect();

    let route = state.route();
    if layout.sidebar().is_empty() && route != UiRoute::Transcript {
        screen.paint_lines(
            transcript_rect,
            &sidebar_lines(route, state, transcript_rect.width(), transcript_rect.height(), cancel),
        );
    } else {
        screen.paint_lines(transcript_rect, &transcript_lines);
        if !layout.sidebar().is_empty() {
            screen.paint_lines(
                layout.sidebar(),
                &sidebar_lines(route, state, layout.sidebar().width(), layout.sidebar().height(), cancel),
            );
        }
    }

    screen.paint_lines(layout.composer(), composer_lines);

    let status = render_status_with(state, chrome, layout.status().width());
    screen.paint_lines(layout.status(), &[status.text()]);

    if modal_open && !layout.modal().is_empty() {
        screen.paint_lines(layout.modal(), &modal_lines(state, layout.modal().width(), layout.modal().height()));
    }

    screen
}

/// Minimal modal content: `AppState`'s own `ApprovalProjection` only ever
/// carries an id/lifecycle/decision (see `crate::state`) — nowhere near the
/// capability/risk/policy/scope detail `ApprovalViewModel` needs to render
/// its full prompt. Rather than inventing that detail, this states the one
/// fact `AppState` genuinely has: which approval is pending. A richer modal
/// is future work, not something this can honestly render from what exists
/// today.
fn modal_lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut lines = Vec::new();
    for modal in state.modal_stack() {
        match modal {
            crate::state::Modal::Approval { id } => {
                lines.push(format!("approval required: {id}"));
                lines.push("resolve from the CLI/host approval surface".to_owned());
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
    for line in &mut lines {
        *line = fit_width(line, usize::from(width));
    }
    lines
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
    use crate::state::{GoalLifecycle, GoalProjection, LocalUiEvent, TranscriptEntry, UiEvent, reduce};
    use protocol::GoalId;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
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
    fn transcript_entries_render_with_distinct_glyphs_and_never_conflate_context_required_with_failure() {
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
        });
        transcript.push_entry(&TranscriptEntry::ToolActivity {
            tool: "ask_user".to_owned(),
            status: crate::state::ToolActivityStatus::ContextRequired,
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

        assert!(transcript_screen.snapshot().contains("hello from the transcript"));
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
    fn modal_open_shows_a_pending_approval_notice() {
        use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
        use protocol::{EventId, RedactionClass, SessionId, TraceId};

        let session: SessionId = "019c0000-0000-7000-8000-000000000010".parse().expect("session");
        let actor = ActorRef::new(ActorKind::System, "019c0000-0000-7000-8000-000000000016")
            .expect("actor");
        let event = |seq: u64, kind: EventKind, payload: serde_json::Value| {
            EventEnvelope::new(
                format!("019c0000-0000-7000-8000-{seq:012x}")
                    .parse::<EventId>()
                    .expect("event id"),
                session,
                seq,
                "2026-08-14T15:20:04.123Z".parse::<RecordedAt>().expect("recorded_at"),
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
        state = reduce(
            state,
            &UiEvent::Kernel(event(
                2,
                EventKind::ToolApprovalRequired,
                serde_json::json!({
                    "turn_id": "019c0000-0000-7000-8000-000000000012",
                    "call_id": "call-1",
                    "tool": "shell_exec",
                    "approval_id": "019c0000-0000-7000-8000-00000000001a",
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
        assert!(
            screen
                .snapshot()
                .contains("approval required: 019c0000-0000-7000-8000-00000000001a"),
            "{}",
            screen.snapshot()
        );
    }
}
