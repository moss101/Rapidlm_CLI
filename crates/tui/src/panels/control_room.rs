//! Control-room panel: one projection over goal, workflow-phase, and child
//! stream state.
//!
//! [`ControlRoomViewModel`] is a frontend read-model like the agents and
//! goals panels: the host copies observations in, the panel renders them
//! deterministically. It never mutates domain state. Goal statements and
//! phase/stream names are untrusted and sanitized; counts are numeric only.

use std::borrow::Cow;
use std::fmt;

use crate::sanitize::sanitize_untrusted;
use crate::state::GoalLifecycle;

/// Maximum phases projected in one room.
pub const MAX_PHASES: usize = 8;
/// Maximum child streams projected in one room.
pub const MAX_STREAMS: usize = 8;
/// Render width is clamped to this many columns.
pub const MAX_ROOM_COLS: u16 = 512;
/// Render height is clamped to this many rows.
pub const MAX_ROOM_ROWS: u16 = 256;
/// Preview truncation for the goal statement.
pub const MAX_GOAL_PREVIEW_CHARS: usize = 40;
/// Bounded phase/stream name length.
pub const MAX_ROOM_NAME_BYTES: usize = 32;

/// Typed panel failure. Display never echoes names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ControlRoomError {
    TooManyPhases { limit: usize },
    TooManyStreams { limit: usize },
    InvalidField,
}

impl fmt::Display for ControlRoomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyPhases { limit } => write!(f, "more than {limit} phases in one room"),
            Self::TooManyStreams { limit } => {
                write!(f, "more than {limit} child streams in one room")
            }
            Self::InvalidField => write!(f, "phase or stream name is invalid"),
        }
    }
}

impl std::error::Error for ControlRoomError {}

/// Projected workflow-phase state. The host maps its node/task states onto
/// this closed set; the panel never interprets domain enums directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RoomPhaseState {
    Pending,
    Ready,
    Running,
    Blocked,
    Done,
    Failed,
}

impl RoomPhaseState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// One projected workflow phase with its child-stream roll-up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomPhase {
    name: String,
    state: RoomPhaseState,
    children_running: u32,
    children_done: u32,
}

impl RoomPhase {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn state(&self) -> RoomPhaseState {
        self.state
    }
    pub const fn children_running(&self) -> u32 {
        self.children_running
    }
    pub const fn children_done(&self) -> u32 {
        self.children_done
    }
}

/// One projected child stream (a spawned subagent or process).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomStream {
    name: String,
    state: RoomPhaseState,
}

impl RoomStream {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn state(&self) -> RoomPhaseState {
        self.state
    }
}

/// The goal summary row at the top of the room.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomGoal {
    statement: String,
    lifecycle: GoalLifecycle,
    turns: u64,
    tokens: u64,
}

impl RoomGoal {
    pub fn statement(&self) -> &str {
        &self.statement
    }
    pub const fn lifecycle(&self) -> GoalLifecycle {
        self.lifecycle
    }
    pub const fn turns(&self) -> u64 {
        self.turns
    }
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }
}

fn sanitize_name(raw: &str) -> Result<String, ControlRoomError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_ROOM_NAME_BYTES {
        return Err(ControlRoomError::InvalidField);
    }
    Ok(match sanitize_untrusted(raw) {
        Cow::Borrowed(safe) => safe.to_string(),
        Cow::Owned(safe) => safe,
    })
}

/// Control-room read-model over goal + workflow + child-stream state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ControlRoomViewModel {
    goal: Option<RoomGoal>,
    phases: Vec<RoomPhase>,
    streams: Vec<RoomStream>,
}

/// Copy a goal observation into the room. The statement is sanitized and
/// preview-truncated here so untrusted text never reaches rendering.
pub fn goal_row(statement: &str, lifecycle: GoalLifecycle, turns: u64, tokens: u64) -> RoomGoal {
    let sanitized: String = match sanitize_untrusted(statement) {
        Cow::Borrowed(safe) => safe.to_string(),
        Cow::Owned(safe) => safe,
    };
    let preview: String = sanitized.chars().take(MAX_GOAL_PREVIEW_CHARS).collect();
    RoomGoal {
        statement: preview,
        lifecycle,
        turns,
        tokens,
    }
}

/// Copy one phase observation into the room.
pub fn phase_row(
    name: &str,
    state: RoomPhaseState,
    children_running: u32,
    children_done: u32,
) -> Result<RoomPhase, ControlRoomError> {
    Ok(RoomPhase {
        name: sanitize_name(name)?,
        state,
        children_running,
        children_done,
    })
}

/// Copy one child-stream observation into the room.
pub fn stream_row(name: &str, state: RoomPhaseState) -> Result<RoomStream, ControlRoomError> {
    Ok(RoomStream {
        name: sanitize_name(name)?,
        state,
    })
}

impl ControlRoomViewModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_goal(&mut self, goal: RoomGoal) {
        self.goal = Some(goal);
    }

    pub fn goal(&self) -> Option<&RoomGoal> {
        self.goal.as_ref()
    }

    pub fn phases(&self) -> &[RoomPhase] {
        &self.phases
    }

    pub fn streams(&self) -> &[RoomStream] {
        &self.streams
    }

    /// Add a phase row. The room is bounded; exceeding the bound is a typed
    /// error, never a silent drop.
    pub fn push_phase(&mut self, phase: RoomPhase) -> Result<(), ControlRoomError> {
        if self.phases.len() >= MAX_PHASES {
            return Err(ControlRoomError::TooManyPhases { limit: MAX_PHASES });
        }
        self.phases.push(phase);
        Ok(())
    }

    /// Add a child-stream row.
    pub fn push_stream(&mut self, stream: RoomStream) -> Result<(), ControlRoomError> {
        if self.streams.len() >= MAX_STREAMS {
            return Err(ControlRoomError::TooManyStreams { limit: MAX_STREAMS });
        }
        self.streams.push(stream);
        Ok(())
    }

    /// Deterministic projection. Widths above the clamp render at the clamp;
    /// zero-sized panes render empty.
    pub fn render(&self, width: u16, height: u16) -> ControlRoomFrame {
        let width = width.min(MAX_ROOM_COLS);
        let height = height.min(MAX_ROOM_ROWS);
        let mut lines: Vec<String> = Vec::new();
        match &self.goal {
            Some(goal) => lines.push(format!(
                "control room goal:{} turns:{} tokens:{} statement:{}",
                goal_lifecycle_label(goal.lifecycle),
                goal.turns,
                goal.tokens,
                goal.statement
            )),
            None => lines.push("control room goal:none".to_string()),
        }
        let running = self
            .phases
            .iter()
            .filter(|p| p.state == RoomPhaseState::Running)
            .count();
        let blocked = self
            .phases
            .iter()
            .filter(|p| p.state == RoomPhaseState::Blocked)
            .count();
        lines.push(format!(
            "phases:{} running:{} blocked:{}",
            self.phases.len(),
            running,
            blocked
        ));
        for phase in &self.phases {
            lines.push(format!(
                "phase {} {} children:{}/{}",
                phase.name,
                phase.state.as_str(),
                phase.children_running,
                phase.children_done
            ));
        }
        for stream in &self.streams {
            lines.push(format!("stream {} {}", stream.name, stream.state.as_str()));
        }
        let height = usize::from(height);
        lines.truncate(height);
        ControlRoomFrame {
            width,
            height: height as u16,
            lines,
        }
    }
}

/// Deterministic lifecycle label for the header line.
fn goal_lifecycle_label(lifecycle: GoalLifecycle) -> &'static str {
    match lifecycle {
        GoalLifecycle::Active => "active",
        GoalLifecycle::Paused => "paused",
        GoalLifecycle::Blocked => "blocked",
        GoalLifecycle::Completed => "completed",
        GoalLifecycle::Cancelled => "cancelled",
    }
}

/// One rendered control-room frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlRoomFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl ControlRoomFrame {
    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    /// Exact logical rows, one per line.
    pub fn golden(&self) -> String {
        self.lines.join("\n")
    }

    /// Exact-width rows written into the pane, padded/truncated to `height`.
    pub fn text(&self) -> String {
        let width = usize::from(self.width);
        let mut out = String::new();
        for index in 0..usize::from(self.height) {
            let mut line = self.lines.get(index).map_or(String::new(), Clone::clone);
            if line.chars().count() > width {
                line = line.chars().take(width).collect();
            } else {
                let pad = width - line.chars().count();
                line.push_str(&" ".repeat(pad));
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_model() -> ControlRoomViewModel {
        let mut model = ControlRoomViewModel::new();
        model.set_goal(goal_row("ship the release", GoalLifecycle::Active, 3, 120));
        model
            .push_phase(phase_row("plan", RoomPhaseState::Done, 0, 2).expect("phase"))
            .expect("push");
        model
            .push_phase(phase_row("build", RoomPhaseState::Running, 1, 1).expect("phase"))
            .expect("push");
        model
            .push_phase(phase_row("verify", RoomPhaseState::Blocked, 0, 0).expect("phase"))
            .expect("push");
        model
            .push_stream(stream_row("builder-1", RoomPhaseState::Running).expect("stream"))
            .expect("push");
        model
            .push_stream(stream_row("reviewer", RoomPhaseState::Blocked).expect("stream"))
            .expect("push");
        model
    }

    const GOLDEN_80: &str = "\
control room goal:active turns:3 tokens:120 statement:ship the release
phases:3 running:1 blocked:1
phase plan done children:0/2
phase build running children:1/1
phase verify blocked children:0/0
stream builder-1 running
stream reviewer blocked";

    #[test]
    fn golden_is_stable_across_widths() {
        let model = fixture_model();
        assert_eq!(model.render(80, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(120, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(200, 24).golden(), GOLDEN_80);
        // Height clamp: rows padded/truncated to the requested height.
        assert_eq!(model.render(80, 24).text().lines().count(), 24);
        assert!(
            model
                .render(80, 3)
                .text()
                .lines()
                .all(|l| l.chars().count() == 80)
        );
        assert_eq!(model.render(80, 0).text(), String::new());
    }

    #[test]
    fn untrusted_text_is_sanitized_and_bounded() {
        let mut model = ControlRoomViewModel::new();
        model.set_goal(goal_row(
            "evil\u{1b}[31mstatement",
            GoalLifecycle::Active,
            0,
            0,
        ));
        let rendered = model.render(80, 8).golden();
        assert!(!rendered.contains('\u{1b}'));
        let long = "x".repeat(MAX_GOAL_PREVIEW_CHARS + 10);
        model.set_goal(goal_row(&long, GoalLifecycle::Active, 0, 0));
        assert_eq!(
            model.goal().expect("goal").statement().chars().count(),
            MAX_GOAL_PREVIEW_CHARS
        );
    }

    #[test]
    fn room_bounds_are_typed_and_names_are_validated() {
        let mut model = ControlRoomViewModel::new();
        for index in 0..MAX_PHASES {
            model
                .push_phase(
                    phase_row(&format!("p{index}"), RoomPhaseState::Pending, 0, 0).expect("row"),
                )
                .expect("push");
        }
        let err = model
            .push_phase(phase_row("extra", RoomPhaseState::Pending, 0, 0).expect("row"))
            .expect_err("bound");
        assert!(matches!(err, ControlRoomError::TooManyPhases { .. }));
        let mut stream_room = ControlRoomViewModel::new();
        for index in 0..MAX_STREAMS {
            stream_room
                .push_stream(
                    stream_row(&format!("s{index}"), RoomPhaseState::Running).expect("row"),
                )
                .expect("push");
        }
        let err = stream_room
            .push_stream(stream_row("extra", RoomPhaseState::Running).expect("row"))
            .expect_err("bound");
        assert!(matches!(err, ControlRoomError::TooManyStreams { .. }));
        assert!(matches!(
            phase_row("", RoomPhaseState::Pending, 0, 0),
            Err(ControlRoomError::InvalidField)
        ));
    }

    #[test]
    fn an_empty_room_renders_the_header_only() {
        let model = ControlRoomViewModel::new();
        assert_eq!(
            model.render(80, 8).golden(),
            "control room goal:none\nphases:0 running:0 blocked:0"
        );
    }
}
