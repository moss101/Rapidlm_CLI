//! Classify in-flight model/tool/turn/goal work after a process restart.
//!
//! [`classify_inflight`] is a pure map from a replayed [`InflightProjection`]
//! to interrupt/pause actions. It never emits success: a completed outcome
//! requires a terminal ledger event already present in the stream.

use std::error::Error;
use std::fmt;

use event_ledger::event::{ErasedEventEnvelope, EventKind};
use protocol::{ApiError, ErrorCode, GoalId, TraceId, TurnId, UNKNOWN_INTERNAL_MESSAGE};
use serde_json::Value;

use crate::CancellationToken;
use crate::session::projection::{
    GoalState, GoalStopReason, MAX_REPLAY_EVENTS, ProjectionError, ProjectionInvariant,
    SessionSnapshot, SessionStatus, apply,
};

/// Maximum concurrent in-flight model requests retained while folding.
pub const MAX_INFLIGHT_MODELS: usize = 256;

/// Maximum concurrent in-flight tool calls retained while folding.
pub const MAX_INFLIGHT_TOOLS: usize = 256;

/// Maximum UTF-8 bytes accepted in a model `request_id` or tool `call_id`.
pub const MAX_INFLIGHT_ID_BYTES: usize = 256;

const CANCEL_CHECK_EVERY: usize = 32;

/// Replay-derived view of still-open work. Not a durable snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InflightProjection {
    snapshot: SessionSnapshot,
    models: Vec<InflightModel>,
    tools: Vec<InflightTool>,
}

/// Model request that has no `model.completed` / `model.failed` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InflightModel {
    request_id: Option<String>,
    last_kind: EventKind,
    last_seq: u64,
}

/// Tool call that has no `tool.completed` / `tool.failed` / `tool.denied` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InflightTool {
    call_id: Option<String>,
    last_kind: EventKind,
    last_seq: u64,
}

/// Conservative recovery steps. Success is not a representable outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    InterruptModel {
        request_id: Option<String>,
        last_seq: u64,
    },
    InterruptTool {
        call_id: Option<String>,
        last_seq: u64,
    },
    InterruptTurn {
        turn_id: TurnId,
    },
    PauseGoal {
        goal_id: GoalId,
        reason: GoalStopReason,
    },
}

/// How recovery will park a unit of work.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RecoveryDisposition {
    Interrupted,
    Paused,
}

/// Ordered interrupt/pause actions for one recovered session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryActions {
    actions: Vec<RecoveryAction>,
}

/// Failure while folding events into an [`InflightProjection`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClassifyError {
    Cancelled,
    TooManyEvents,
    TooManyInflight,
    InvalidField { field: &'static str },
    FieldTooLong { field: &'static str },
    Projection(ProjectionError),
}

/// Fold committed events into the in-flight recovery view.
pub fn project_inflight(
    events: &[ErasedEventEnvelope],
    cancel: &CancellationToken,
) -> Result<InflightProjection, ClassifyError> {
    if events.len() > MAX_REPLAY_EVENTS {
        return Err(ClassifyError::TooManyEvents);
    }
    check_cancel(cancel)?;
    let mut snapshot = None;
    let mut models = Vec::new();
    let mut tools = Vec::new();
    for (i, event) in events.iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            check_cancel(cancel)?;
        }
        snapshot = Some(apply(snapshot, event).map_err(ClassifyError::Projection)?);
        apply_inflight(&mut models, &mut tools, event)?;
    }
    let snapshot = snapshot.ok_or(ClassifyError::Projection(ProjectionError::Invariant(
        ProjectionInvariant::SessionNotCreated,
    )))?;
    Ok(InflightProjection {
        snapshot,
        models,
        tools,
    })
}

/// Map open work to interrupt/pause actions. Never marks work succeeded.
pub fn classify_inflight(projection: &InflightProjection) -> RecoveryActions {
    if projection.snapshot.status() == SessionStatus::Closed {
        return RecoveryActions::default();
    }

    let mut actions = Vec::with_capacity(
        projection
            .models
            .len()
            .saturating_add(projection.tools.len())
            .saturating_add(2),
    );

    let mut models = projection.models.clone();
    models.sort_by_key(|model| model.last_seq);
    for model in models {
        actions.push(RecoveryAction::InterruptModel {
            request_id: model.request_id,
            last_seq: model.last_seq,
        });
    }

    let mut tools = projection.tools.clone();
    tools.sort_by_key(|tool| tool.last_seq);
    for tool in tools {
        actions.push(RecoveryAction::InterruptTool {
            call_id: tool.call_id,
            last_seq: tool.last_seq,
        });
    }

    if let Some(turn_id) = projection.snapshot.active_turn() {
        actions.push(RecoveryAction::InterruptTurn { turn_id });
    }

    if let Some(goal) = projection.snapshot.top_level_goal()
        && goal.state() == GoalState::Active {
            actions.push(RecoveryAction::PauseGoal {
                goal_id: goal.id(),
                reason: GoalStopReason::ProcessRecovered,
            });
        }

    RecoveryActions { actions }
}

fn apply_inflight(
    models: &mut Vec<InflightModel>,
    tools: &mut Vec<InflightTool>,
    event: &ErasedEventEnvelope,
) -> Result<(), ClassifyError> {
    match event.kind() {
        EventKind::SessionClosed => {
            models.clear();
            tools.clear();
            Ok(())
        }
        EventKind::ModelRequested | EventKind::ModelStreamDelta => {
            upsert_model(models, event, optional_id(event.payload(), "request_id")?)
        }
        EventKind::ModelCompleted | EventKind::ModelFailed => {
            remove_model(models, optional_id(event.payload(), "request_id")?);
            Ok(())
        }
        EventKind::ToolRequested
        | EventKind::ToolAuthorized
        | EventKind::ToolApprovalRequired
        | EventKind::ToolStarted => {
            upsert_tool(tools, event, optional_id(event.payload(), "call_id")?)
        }
        EventKind::ToolCompleted | EventKind::ToolFailed | EventKind::ToolDenied => {
            remove_tool(tools, optional_id(event.payload(), "call_id")?);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn upsert_model(
    models: &mut Vec<InflightModel>,
    event: &ErasedEventEnvelope,
    request_id: Option<String>,
) -> Result<(), ClassifyError> {
    if let Some(existing) = find_model_mut(models, request_id.as_deref()) {
        existing.last_kind = event.kind();
        existing.last_seq = event.seq();
        return Ok(());
    }
    if models.len() >= MAX_INFLIGHT_MODELS {
        return Err(ClassifyError::TooManyInflight);
    }
    models.push(InflightModel {
        request_id,
        last_kind: event.kind(),
        last_seq: event.seq(),
    });
    Ok(())
}

fn upsert_tool(
    tools: &mut Vec<InflightTool>,
    event: &ErasedEventEnvelope,
    call_id: Option<String>,
) -> Result<(), ClassifyError> {
    if let Some(existing) = find_tool_mut(tools, call_id.as_deref()) {
        existing.last_kind = event.kind();
        existing.last_seq = event.seq();
        return Ok(());
    }
    if tools.len() >= MAX_INFLIGHT_TOOLS {
        return Err(ClassifyError::TooManyInflight);
    }
    tools.push(InflightTool {
        call_id,
        last_kind: event.kind(),
        last_seq: event.seq(),
    });
    Ok(())
}

fn remove_model(models: &mut Vec<InflightModel>, request_id: Option<String>) {
    if let Some(index) = find_model_index(models, request_id.as_deref()) {
        models.remove(index);
    }
}

fn remove_tool(tools: &mut Vec<InflightTool>, call_id: Option<String>) {
    if let Some(index) = find_tool_index(tools, call_id.as_deref()) {
        tools.remove(index);
    }
}

fn find_model_mut<'a>(
    models: &'a mut [InflightModel],
    request_id: Option<&str>,
) -> Option<&'a mut InflightModel> {
    find_model_index(models, request_id).map(|index| &mut models[index])
}

fn find_tool_mut<'a>(
    tools: &'a mut [InflightTool],
    call_id: Option<&str>,
) -> Option<&'a mut InflightTool> {
    find_tool_index(tools, call_id).map(|index| &mut tools[index])
}

fn find_model_index(models: &[InflightModel], request_id: Option<&str>) -> Option<usize> {
    models
        .iter()
        .position(|model| model.request_id.as_deref() == request_id)
}

fn find_tool_index(tools: &[InflightTool], call_id: Option<&str>) -> Option<usize> {
    tools
        .iter()
        .position(|tool| tool.call_id.as_deref() == call_id)
}

fn optional_id(payload: &Value, field: &'static str) -> Result<Option<String>, ClassifyError> {
    match payload.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => {
            if raw.len() > MAX_INFLIGHT_ID_BYTES {
                return Err(ClassifyError::FieldTooLong { field });
            }
            if raw.is_empty() {
                return Err(ClassifyError::InvalidField { field });
            }
            Ok(Some(raw.clone()))
        }
        Some(_) => Err(ClassifyError::InvalidField { field }),
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ClassifyError> {
    if cancel.is_cancelled() {
        Err(ClassifyError::Cancelled)
    } else {
        Ok(())
    }
}

impl InflightProjection {
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub fn models(&self) -> &[InflightModel] {
        &self.models
    }

    pub fn tools(&self) -> &[InflightTool] {
        &self.tools
    }
}

impl InflightModel {
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub fn last_kind(&self) -> EventKind {
        self.last_kind
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }
}

impl InflightTool {
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    pub fn last_kind(&self) -> EventKind {
        self.last_kind
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }
}

impl RecoveryAction {
    pub fn disposition(&self) -> RecoveryDisposition {
        match self {
            Self::PauseGoal { .. } => RecoveryDisposition::Paused,
            Self::InterruptModel { .. }
            | Self::InterruptTool { .. }
            | Self::InterruptTurn { .. } => RecoveryDisposition::Interrupted,
        }
    }

    /// Recovery never fabricates a terminal success.
    pub fn marks_succeeded(&self) -> bool {
        false
    }
}

impl RecoveryActions {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }

    pub fn as_slice(&self) -> &[RecoveryAction] {
        &self.actions
    }

    pub fn iter(&self) -> impl Iterator<Item = &RecoveryAction> {
        self.actions.iter()
    }

    pub fn contains_success(&self) -> bool {
        self.actions.iter().any(RecoveryAction::marks_succeeded)
    }
}

impl RecoveryDisposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Paused => "paused",
        }
    }
}

impl ClassifyError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`ClassifyError::Cancelled`] has no public code.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::TooManyEvents | Self::TooManyInflight => Some(ErrorCode::InternalUnexpected),
            Self::InvalidField { .. } | Self::FieldTooLong { .. } => {
                Some(ErrorCode::StorageCorrupt)
            }
            Self::Projection(ProjectionError::Cancelled) => None,
            Self::Projection(ProjectionError::TooManyEvents) => Some(ErrorCode::InternalUnexpected),
            Self::Projection(ProjectionError::Invariant(_)) => Some(ErrorCode::StorageCorrupt),
        }
    }

    /// Convert to the public envelope. Cancellation is not an API error.
    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match &self {
            Self::Cancelled | Self::Projection(ProjectionError::Cancelled) => return None,
            Self::TooManyEvents
            | Self::TooManyInflight
            | Self::Projection(ProjectionError::TooManyEvents) => UNKNOWN_INTERNAL_MESSAGE,
            Self::InvalidField { .. }
            | Self::FieldTooLong { .. }
            | Self::Projection(ProjectionError::Invariant(_)) => "Session store is corrupt",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }
}

impl fmt::Display for RecoveryDisposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for RecoveryAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InterruptModel {
                request_id,
                last_seq,
            } => match request_id {
                Some(id) => write!(f, "interrupt model {id} at seq {last_seq}"),
                None => write!(f, "interrupt model at seq {last_seq}"),
            },
            Self::InterruptTool { call_id, last_seq } => match call_id {
                Some(id) => write!(f, "interrupt tool {id} at seq {last_seq}"),
                None => write!(f, "interrupt tool at seq {last_seq}"),
            },
            Self::InterruptTurn { turn_id } => write!(f, "interrupt turn {turn_id}"),
            Self::PauseGoal { goal_id, reason } => {
                write!(f, "pause goal {goal_id} ({})", reason.as_str())
            }
        }
    }
}

impl fmt::Display for ClassifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("recovery classify cancelled"),
            Self::TooManyEvents => f.write_str("recovery classify exceeds the event bound"),
            Self::TooManyInflight => f.write_str("recovery classify exceeds the in-flight bound"),
            Self::InvalidField { field } => write!(f, "event payload has invalid field {field}"),
            Self::FieldTooLong { field } => {
                write!(f, "event payload field {field} exceeds bound")
            }
            Self::Projection(inner) => inner.fmt(f),
        }
    }
}

impl Error for ClassifyError {}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, RecordedAt};
    use protocol::{EventId, RedactionClass};
    use serde_json::json;

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
    const PROJECT_ID: &str = "019c0000-0000-7000-8000-000000000011";
    const TURN_ID: &str = "019c0000-0000-7000-8000-000000000012";
    const GOAL_ID: &str = "019c0000-0000-7000-8000-000000000014";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000017";
    const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
    const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";
    const MODEL_REQUEST: &str = "model-req-1";
    const TOOL_CALL: &str = "call_7";

    fn envelope(seq: u64, kind: EventKind, payload: Value) -> ErasedEventEnvelope {
        let at = if seq == 1 { CREATED_AT } else { UPDATED_AT };
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

    fn created() -> ErasedEventEnvelope {
        envelope(
            1,
            EventKind::SessionCreated,
            json!({"project_id": PROJECT_ID}),
        )
    }

    fn turn_started(seq: u64) -> ErasedEventEnvelope {
        envelope(seq, EventKind::TurnStarted, json!({"turn_id": TURN_ID}))
    }

    fn goal_created(seq: u64) -> ErasedEventEnvelope {
        envelope(
            seq,
            EventKind::GoalCreated,
            json!({"goal_id": GOAL_ID, "statement": "recover safely"}),
        )
    }

    fn classify(events: &[ErasedEventEnvelope]) -> (InflightProjection, RecoveryActions) {
        let projection =
            project_inflight(events, &CancellationToken::new()).expect("project_inflight");
        let actions = classify_inflight(&projection);
        assert!(!actions.contains_success());
        assert!(actions.iter().all(|action| !action.marks_succeeded()));
        (projection, actions)
    }

    #[test]
    fn crash_during_model_stream_is_interrupted() {
        let events = vec![
            created(),
            turn_started(2),
            envelope(
                3,
                EventKind::ModelRequested,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(
                4,
                EventKind::ModelStreamDelta,
                json!({"request_id": MODEL_REQUEST, "chunk": "partial"}),
            ),
        ];
        let (projection, actions) = classify(&events);
        assert_eq!(projection.models().len(), 1);
        assert_eq!(
            projection.models()[0].last_kind(),
            EventKind::ModelStreamDelta
        );
        assert_eq!(
            actions.as_slice(),
            &[
                RecoveryAction::InterruptModel {
                    request_id: Some(MODEL_REQUEST.to_owned()),
                    last_seq: 4,
                },
                RecoveryAction::InterruptTurn {
                    turn_id: TURN_ID.parse().expect("turn"),
                },
            ]
        );
        assert_eq!(
            actions
                .iter()
                .map(RecoveryAction::disposition)
                .collect::<Vec<_>>(),
            vec![
                RecoveryDisposition::Interrupted,
                RecoveryDisposition::Interrupted,
            ]
        );
    }

    #[test]
    fn crash_during_tool_execution_is_interrupted() {
        let events = vec![
            created(),
            turn_started(2),
            envelope(3, EventKind::ToolRequested, json!({"call_id": TOOL_CALL})),
            envelope(4, EventKind::ToolAuthorized, json!({"call_id": TOOL_CALL})),
            envelope(5, EventKind::ToolStarted, json!({"call_id": TOOL_CALL})),
        ];
        let (projection, actions) = classify(&events);
        assert_eq!(projection.tools().len(), 1);
        assert_eq!(projection.tools()[0].last_kind(), EventKind::ToolStarted);
        assert_eq!(
            actions.as_slice(),
            &[
                RecoveryAction::InterruptTool {
                    call_id: Some(TOOL_CALL.to_owned()),
                    last_seq: 5,
                },
                RecoveryAction::InterruptTurn {
                    turn_id: TURN_ID.parse().expect("turn"),
                },
            ]
        );
    }

    #[test]
    fn crash_during_goal_turn_pauses_goal_and_interrupts_turn() {
        let events = vec![created(), goal_created(2), turn_started(3)];
        let (projection, actions) = classify(&events);
        assert_eq!(
            projection
                .snapshot()
                .top_level_goal()
                .expect("goal")
                .state(),
            GoalState::Active
        );
        assert_eq!(
            actions.as_slice(),
            &[
                RecoveryAction::InterruptTurn {
                    turn_id: TURN_ID.parse().expect("turn"),
                },
                RecoveryAction::PauseGoal {
                    goal_id: GOAL_ID.parse().expect("goal"),
                    reason: GoalStopReason::ProcessRecovered,
                },
            ]
        );
        assert_eq!(
            actions.as_slice()[1].disposition(),
            RecoveryDisposition::Paused
        );
    }

    #[test]
    fn terminal_events_are_not_marked_succeeded() {
        let events = vec![
            created(),
            goal_created(2),
            turn_started(3),
            envelope(
                4,
                EventKind::ModelRequested,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(
                5,
                EventKind::ModelCompleted,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(6, EventKind::ToolStarted, json!({"call_id": TOOL_CALL})),
            envelope(
                7,
                EventKind::ToolCompleted,
                json!({"call_id": TOOL_CALL, "status": "ok"}),
            ),
            envelope(8, EventKind::TurnCompleted, json!({"turn_id": TURN_ID})),
            envelope(9, EventKind::GoalCompleted, json!({"goal_id": GOAL_ID})),
        ];
        let (projection, actions) = classify(&events);
        assert!(projection.models().is_empty());
        assert!(projection.tools().is_empty());
        assert!(projection.snapshot().active_turn().is_none());
        assert!(projection.snapshot().top_level_goal().is_none());
        assert!(actions.is_empty());
        assert!(!actions.contains_success());
    }

    #[test]
    fn failed_and_denied_are_not_succeeded() {
        let events = vec![
            created(),
            turn_started(2),
            envelope(
                3,
                EventKind::ModelRequested,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(
                4,
                EventKind::ModelFailed,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(5, EventKind::ToolStarted, json!({"call_id": TOOL_CALL})),
            envelope(6, EventKind::ToolDenied, json!({"call_id": TOOL_CALL})),
            envelope(7, EventKind::TurnFailed, json!({"turn_id": TURN_ID})),
        ];
        let (_projection, actions) = classify(&events);
        assert!(actions.is_empty());
        assert!(actions.iter().all(|action| !action.marks_succeeded()));
    }

    #[test]
    fn unmatched_terminal_does_not_clear_other_inflight_or_fabricate_success() {
        let events = vec![
            created(),
            turn_started(2),
            envelope(
                3,
                EventKind::ModelRequested,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(
                4,
                EventKind::ModelCompleted,
                json!({"request_id": "other-request"}),
            ),
        ];
        let (projection, actions) = classify(&events);
        assert_eq!(projection.models().len(), 1);
        assert_eq!(projection.models()[0].request_id(), Some(MODEL_REQUEST));
        assert!(matches!(
            &actions.as_slice()[0],
            RecoveryAction::InterruptModel { request_id, .. }
                if request_id.as_deref() == Some(MODEL_REQUEST)
        ));
        assert!(!actions.contains_success());
    }

    #[test]
    fn paused_goal_is_not_reclassified() {
        let events = vec![
            created(),
            goal_created(2),
            envelope(3, EventKind::GoalPaused, json!({"goal_id": GOAL_ID})),
        ];
        let (_projection, actions) = classify(&events);
        assert!(actions.is_empty());
    }

    #[test]
    fn closed_session_emits_no_actions() {
        let events = vec![
            created(),
            turn_started(2),
            envelope(
                3,
                EventKind::ModelRequested,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(4, EventKind::SessionClosed, json!({})),
        ];
        let (projection, actions) = classify(&events);
        assert_eq!(projection.snapshot().status(), SessionStatus::Closed);
        assert!(projection.models().is_empty());
        assert!(actions.is_empty());
    }

    #[test]
    fn classify_is_deterministic() {
        let events = vec![
            created(),
            goal_created(2),
            turn_started(3),
            envelope(
                4,
                EventKind::ModelStreamDelta,
                json!({"request_id": MODEL_REQUEST}),
            ),
            envelope(5, EventKind::ToolStarted, json!({"call_id": TOOL_CALL})),
        ];
        let first = classify(&events).1;
        let second = classify(&events).1;
        assert_eq!(first, second);
        assert_eq!(first.len(), 4);
    }

    #[test]
    fn project_inflight_observes_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = project_inflight(&[created()], &cancel).expect_err("cancelled");
        assert_eq!(err, ClassifyError::Cancelled);
        assert!(err.code().is_none());
        assert!(err.into_api_error(TraceId::new()).is_none());
    }

    #[test]
    fn invalid_request_id_is_typed() {
        let events = vec![
            created(),
            envelope(2, EventKind::ModelRequested, json!({"request_id": 1})),
        ];
        let err = project_inflight(&events, &CancellationToken::new()).expect_err("invalid");
        assert_eq!(
            err,
            ClassifyError::InvalidField {
                field: "request_id"
            }
        );
        let mapped = err.into_api_error(TraceId::new()).expect("mapped");
        assert_eq!(mapped.code(), ErrorCode::StorageCorrupt);
    }
}
