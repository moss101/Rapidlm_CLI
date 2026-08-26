//! Deterministic session projection.
//!
//! `apply` is a pure fold step: it reads only the prior snapshot and the
//! event envelope. It does not perform I/O or consult the wall clock.
//! Timestamps on the snapshot are copied from `event.recorded_at`.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use event_ledger::event::{ErasedEventEnvelope, EventKind};
use protocol::{AgentId, GoalId, IdParseError, ProjectId, SessionId, TurnId};
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CancellationToken;

/// Projection document schema written on every constructed snapshot.
pub const SESSION_SNAPSHOT_SCHEMA: u16 = 1;

/// Maximum events accepted by [`replay`].
pub const MAX_REPLAY_EVENTS: usize = 1_048_576;

/// Maximum agents tracked as active on one snapshot.
pub const MAX_ACTIVE_AGENTS: usize = 256;

/// Maximum UTF-8 bytes accepted in a goal statement.
pub const MAX_GOAL_STATEMENT_BYTES: usize = 16 * 1024;

/// Maximum completion criteria retained on a goal snapshot.
pub const MAX_CRITERIA: usize = 64;

/// Maximum UTF-8 bytes accepted in one criterion identifier or text.
pub const MAX_CRITERION_TEXT_BYTES: usize = 4 * 1024;

/// Maximum evidence-requirement entries retained on a goal snapshot.
pub const MAX_EVIDENCE_REQUIREMENTS: usize = 64;

const CANCEL_CHECK_EVERY: usize = 32;

const SNAPSHOT_FIELDS: &[&str] = &[
    "schema",
    "id",
    "project_id",
    "status",
    "active_turn",
    "top_level_goal",
    "active_agents",
    "seq",
    "created_at",
    "updated_at",
];

/// Derived session view. Rebuildable from the session event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    schema: u16,
    id: SessionId,
    project_id: ProjectId,
    status: SessionStatus,
    active_turn: Option<TurnId>,
    top_level_goal: Option<GoalSnapshot>,
    active_agents: Vec<AgentId>,
    seq: u64,
    created_at: String,
    updated_at: String,
}

/// Session lifecycle observed by the projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SessionStatus {
    Ready,
    Busy,
    Paused,
    Recovering,
    Closed,
}

/// Top-level goal retained on the session projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GoalSnapshot {
    id: GoalId,
    statement: String,
    completion_criteria: Vec<Criterion>,
    state: GoalState,
    stop_reason: Option<GoalStopReason>,
    budget: GoalBudget,
    usage: GoalUsage,
    evidence_requirements: Vec<EvidenceRequirement>,
}

/// Durable top-level goal lifecycle. Completion/cancel clear the snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GoalState {
    Active,
    Paused,
    Blocked,
}

/// Why a still-projected goal stopped making progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GoalStopReason {
    Completed,
    Cancelled,
    BudgetExhausted,
    ProcessRecovered,
}

/// One completion criterion copied from a goal event payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    id: String,
    text: String,
}

/// Configured goal ceilings. Absent fields are unbounded.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GoalBudget {
    max_turns: Option<u64>,
    max_tokens: Option<u64>,
}

/// Observed goal consumption. Defaults are zero.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GoalUsage {
    turns: u64,
    tokens: u64,
}

/// Evidence expected before a criterion may pass.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRequirement {
    criterion_id: String,
    kinds: Vec<String>,
}

/// Failure when applying or replaying events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    Cancelled,
    TooManyEvents,
    Invariant(ProjectionInvariant),
}

/// Illegal event relative to the current projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionInvariant {
    SessionNotCreated,
    SessionAlreadyCreated,
    SessionClosed,
    SessionIdMismatch {
        expected: SessionId,
        found: SessionId,
    },
    SeqGap {
        expected: u64,
        found: u64,
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
    TooManyCriteria,
    TooManyEvidenceRequirements,
    TurnAlreadyActive,
    TurnNotActive,
    TurnMismatch {
        expected: TurnId,
        found: TurnId,
    },
    GoalAlreadyActive,
    GoalNotActive,
    GoalMismatch {
        expected: GoalId,
        found: GoalId,
    },
    GoalInvalidTransition {
        from: GoalState,
        kind: EventKind,
    },
    AgentAlreadyActive,
    AgentNotActive,
    AgentLimit,
}

/// Apply one committed event to a projection.
///
/// `snapshot` is `None` only before the first event, which must be
/// `session.created` or `session.forked` at seq 1.
pub fn apply(
    snapshot: Option<SessionSnapshot>,
    event: &ErasedEventEnvelope,
) -> Result<SessionSnapshot, ProjectionError> {
    match snapshot {
        None => apply_first(event),
        Some(current) => apply_next(current, event),
    }
}

/// Fold [`apply`] over `events`. The same list always yields a byte-equal snapshot.
pub fn replay(
    events: &[ErasedEventEnvelope],
    cancel: &CancellationToken,
) -> Result<SessionSnapshot, ProjectionError> {
    if events.len() > MAX_REPLAY_EVENTS {
        return Err(ProjectionError::TooManyEvents);
    }
    check_cancel(cancel)?;
    let mut snapshot = None;
    for (i, event) in events.iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            check_cancel(cancel)?;
        }
        snapshot = Some(apply(snapshot, event)?);
    }
    snapshot.ok_or(ProjectionError::Invariant(
        ProjectionInvariant::SessionNotCreated,
    ))
}

fn apply_first(event: &ErasedEventEnvelope) -> Result<SessionSnapshot, ProjectionError> {
    if event.seq() != 1 {
        return Err(invariant(ProjectionInvariant::SeqGap {
            expected: 1,
            found: event.seq(),
        }));
    }
    match event.kind() {
        EventKind::SessionCreated | EventKind::SessionForked => {
            let project_id = parse_id(event.payload(), "project_id")?;
            let recorded_at = event.recorded_at().as_str().to_owned();
            Ok(SessionSnapshot {
                schema: SESSION_SNAPSHOT_SCHEMA,
                id: event.session_id(),
                project_id,
                status: SessionStatus::Ready,
                active_turn: None,
                top_level_goal: None,
                active_agents: Vec::new(),
                seq: event.seq(),
                created_at: recorded_at.clone(),
                updated_at: recorded_at,
            })
        }
        _ => Err(invariant(ProjectionInvariant::SessionNotCreated)),
    }
}

fn apply_next(
    mut snapshot: SessionSnapshot,
    event: &ErasedEventEnvelope,
) -> Result<SessionSnapshot, ProjectionError> {
    if event.session_id() != snapshot.id {
        return Err(invariant(ProjectionInvariant::SessionIdMismatch {
            expected: snapshot.id,
            found: event.session_id(),
        }));
    }
    let expected = snapshot.seq.checked_add(1).ok_or_else(|| {
        invariant(ProjectionInvariant::SeqGap {
            expected: u64::MAX,
            found: event.seq(),
        })
    })?;
    if event.seq() != expected {
        return Err(invariant(ProjectionInvariant::SeqGap {
            expected,
            found: event.seq(),
        }));
    }
    if snapshot.status == SessionStatus::Closed {
        return Err(invariant(ProjectionInvariant::SessionClosed));
    }

    snapshot.seq = event.seq();
    snapshot.updated_at = event.recorded_at().as_str().to_owned();

    match event.kind() {
        EventKind::SessionCreated | EventKind::SessionForked => {
            return Err(invariant(ProjectionInvariant::SessionAlreadyCreated));
        }
        EventKind::SessionClosed => {
            snapshot.active_turn = None;
            snapshot.status = SessionStatus::Closed;
            return Ok(snapshot);
        }
        EventKind::SessionRecovered => {
            snapshot.status = if inflight_unresolved(&snapshot) {
                SessionStatus::Recovering
            } else {
                derived_status(&snapshot)
            };
            return Ok(snapshot);
        }
        EventKind::TurnStarted => {
            if snapshot.active_turn.is_some() {
                return Err(invariant(ProjectionInvariant::TurnAlreadyActive));
            }
            snapshot.active_turn = Some(parse_id(event.payload(), "turn_id")?);
        }
        EventKind::TurnCompleted | EventKind::TurnInterrupted | EventKind::TurnFailed => {
            let turn_id: TurnId = parse_id(event.payload(), "turn_id")?;
            match snapshot.active_turn {
                None => return Err(invariant(ProjectionInvariant::TurnNotActive)),
                Some(expected) if expected != turn_id => {
                    return Err(invariant(ProjectionInvariant::TurnMismatch {
                        expected,
                        found: turn_id,
                    }));
                }
                Some(_) => snapshot.active_turn = None,
            }
        }
        EventKind::GoalCreated => apply_goal_created(&mut snapshot, event.payload())?,
        EventKind::GoalUpdated => apply_goal_updated(&mut snapshot, event.payload())?,
        EventKind::GoalBudgetUpdated => apply_goal_budget(&mut snapshot, event.payload())?,
        EventKind::GoalPaused => {
            apply_goal_state(
                &mut snapshot,
                event.payload(),
                EventKind::GoalPaused,
                GoalState::Paused,
            )?;
        }
        EventKind::GoalBlocked => {
            apply_goal_state(
                &mut snapshot,
                event.payload(),
                EventKind::GoalBlocked,
                GoalState::Blocked,
            )?;
        }
        EventKind::GoalResumed => {
            apply_goal_state(
                &mut snapshot,
                event.payload(),
                EventKind::GoalResumed,
                GoalState::Active,
            )?;
        }
        EventKind::GoalCompleted => {
            clear_goal(&mut snapshot, event.payload())?;
        }
        EventKind::GoalCancelled => {
            clear_goal(&mut snapshot, event.payload())?;
        }
        EventKind::AgentSpawned | EventKind::AgentPoolBackgroundStarted => {
            insert_agent(&mut snapshot, parse_id(event.payload(), "agent_id")?)?;
        }
        EventKind::AgentStarted | EventKind::AgentStateChanged => {
            let agent_id: AgentId = parse_id(event.payload(), "agent_id")?;
            if let Some(terminal) = optional_agent_terminal(event.payload())? {
                if terminal {
                    remove_agent(&mut snapshot, agent_id)?;
                } else {
                    require_agent(&snapshot, agent_id)?;
                }
            } else {
                require_agent(&snapshot, agent_id)?;
            }
        }
        EventKind::AgentResult
        | EventKind::AgentCancelled
        | EventKind::AgentPoolBackgroundParked => {
            remove_agent(&mut snapshot, parse_id(event.payload(), "agent_id")?)?;
        }
        _ => {}
    }

    if snapshot.status != SessionStatus::Recovering || !inflight_unresolved(&snapshot) {
        snapshot.status = derived_status(&snapshot);
    }
    Ok(snapshot)
}

fn apply_goal_created(
    snapshot: &mut SessionSnapshot,
    payload: &Value,
) -> Result<(), ProjectionError> {
    if snapshot.top_level_goal.is_some() {
        return Err(invariant(ProjectionInvariant::GoalAlreadyActive));
    }
    let id = parse_id(payload, "goal_id")?;
    let statement = parse_statement(payload)?;
    let completion_criteria = parse_criteria(payload)?;
    let budget = parse_budget(payload)?;
    let usage = parse_usage(payload)?;
    let evidence_requirements = parse_evidence_requirements(payload)?;
    snapshot.top_level_goal = Some(GoalSnapshot {
        id,
        statement,
        completion_criteria,
        state: GoalState::Active,
        stop_reason: None,
        budget,
        usage,
        evidence_requirements,
    });
    Ok(())
}

fn apply_goal_updated(
    snapshot: &mut SessionSnapshot,
    payload: &Value,
) -> Result<(), ProjectionError> {
    let goal = require_goal_mut(snapshot, payload)?;
    if payload.get("statement").is_some() {
        goal.statement = parse_statement(payload)?;
    }
    if payload.get("completion_criteria").is_some() {
        goal.completion_criteria = parse_criteria(payload)?;
    }
    if payload.get("budget").is_some() || payload.get("max_turns").is_some() {
        goal.budget = parse_budget(payload)?;
    }
    if payload.get("usage").is_some() {
        goal.usage = parse_usage(payload)?;
    }
    if payload.get("evidence_requirements").is_some() {
        goal.evidence_requirements = parse_evidence_requirements(payload)?;
    }
    Ok(())
}

fn apply_goal_budget(
    snapshot: &mut SessionSnapshot,
    payload: &Value,
) -> Result<(), ProjectionError> {
    let goal = require_goal_mut(snapshot, payload)?;
    goal.budget = parse_budget(payload)?;
    if payload.get("usage").is_some() {
        goal.usage = parse_usage(payload)?;
    }
    Ok(())
}

fn apply_goal_state(
    snapshot: &mut SessionSnapshot,
    payload: &Value,
    kind: EventKind,
    next: GoalState,
) -> Result<(), ProjectionError> {
    let goal = require_goal_mut(snapshot, payload)?;
    if goal.state == next {
        return Err(invariant(ProjectionInvariant::GoalInvalidTransition {
            from: goal.state,
            kind,
        }));
    }
    goal.state = next;
    goal.stop_reason = match next {
        GoalState::Active => None,
        GoalState::Paused if payload_flag(payload, "process_recovered") => {
            Some(GoalStopReason::ProcessRecovered)
        }
        GoalState::Paused => None,
        GoalState::Blocked if payload_flag(payload, "budget_exhausted") => {
            Some(GoalStopReason::BudgetExhausted)
        }
        GoalState::Blocked => None,
    };
    Ok(())
}

fn clear_goal(snapshot: &mut SessionSnapshot, payload: &Value) -> Result<(), ProjectionError> {
    let _ = require_goal_mut(snapshot, payload)?;
    snapshot.top_level_goal = None;
    Ok(())
}

fn require_goal_mut<'a>(
    snapshot: &'a mut SessionSnapshot,
    payload: &Value,
) -> Result<&'a mut GoalSnapshot, ProjectionError> {
    let found: GoalId = parse_id(payload, "goal_id")?;
    let Some(goal) = snapshot.top_level_goal.as_mut() else {
        return Err(invariant(ProjectionInvariant::GoalNotActive));
    };
    if goal.id != found {
        return Err(invariant(ProjectionInvariant::GoalMismatch {
            expected: goal.id,
            found,
        }));
    }
    Ok(goal)
}

fn insert_agent(snapshot: &mut SessionSnapshot, agent_id: AgentId) -> Result<(), ProjectionError> {
    if snapshot.active_agents.contains(&agent_id) {
        return Err(invariant(ProjectionInvariant::AgentAlreadyActive));
    }
    if snapshot.active_agents.len() >= MAX_ACTIVE_AGENTS {
        return Err(invariant(ProjectionInvariant::AgentLimit));
    }
    snapshot.active_agents.push(agent_id);
    Ok(())
}

fn require_agent(snapshot: &SessionSnapshot, agent_id: AgentId) -> Result<(), ProjectionError> {
    if snapshot.active_agents.contains(&agent_id) {
        Ok(())
    } else {
        Err(invariant(ProjectionInvariant::AgentNotActive))
    }
}

fn remove_agent(snapshot: &mut SessionSnapshot, agent_id: AgentId) -> Result<(), ProjectionError> {
    let Some(index) = snapshot.active_agents.iter().position(|id| *id == agent_id) else {
        return Err(invariant(ProjectionInvariant::AgentNotActive));
    };
    snapshot.active_agents.remove(index);
    Ok(())
}

fn optional_agent_terminal(payload: &Value) -> Result<Option<bool>, ProjectionError> {
    match payload.get("state") {
        None => Ok(None),
        Some(Value::String(state)) => Ok(Some(matches!(
            state.as_str(),
            "succeeded" | "failed" | "cancelled"
        ))),
        Some(_) => Err(invariant(ProjectionInvariant::InvalidField {
            field: "state",
        })),
    }
}

fn inflight_unresolved(snapshot: &SessionSnapshot) -> bool {
    snapshot.active_turn.is_some()
        || snapshot
            .top_level_goal
            .as_ref()
            .is_some_and(|goal| goal.state == GoalState::Active)
}

fn derived_status(snapshot: &SessionSnapshot) -> SessionStatus {
    if snapshot.active_turn.is_some() {
        SessionStatus::Busy
    } else if snapshot
        .top_level_goal
        .as_ref()
        .is_some_and(|goal| matches!(goal.state, GoalState::Paused | GoalState::Blocked))
    {
        SessionStatus::Paused
    } else {
        SessionStatus::Ready
    }
}

fn parse_id<T: FromStr<Err = IdParseError>>(
    payload: &Value,
    field: &'static str,
) -> Result<T, ProjectionError> {
    let Some(raw) = payload.get(field).and_then(Value::as_str) else {
        return Err(invariant(ProjectionInvariant::MissingField { field }));
    };
    raw.parse()
        .map_err(|_| invariant(ProjectionInvariant::InvalidField { field }))
}

fn parse_statement(payload: &Value) -> Result<String, ProjectionError> {
    let Some(value) = payload.get("statement") else {
        return Err(invariant(ProjectionInvariant::MissingField {
            field: "statement",
        }));
    };
    let Some(text) = value.as_str() else {
        return Err(invariant(ProjectionInvariant::InvalidField {
            field: "statement",
        }));
    };
    if text.len() > MAX_GOAL_STATEMENT_BYTES {
        return Err(invariant(ProjectionInvariant::FieldTooLong {
            field: "statement",
        }));
    }
    Ok(text.to_owned())
}

fn parse_criteria(payload: &Value) -> Result<Vec<Criterion>, ProjectionError> {
    let Some(value) = payload.get("completion_criteria") else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(invariant(ProjectionInvariant::InvalidField {
            field: "completion_criteria",
        }));
    };
    if items.len() > MAX_CRITERIA {
        return Err(invariant(ProjectionInvariant::TooManyCriteria));
    }
    let mut criteria = Vec::with_capacity(items.len());
    for item in items {
        let id = bounded_string(item, "id", "completion_criteria")?;
        let text = bounded_string(item, "text", "completion_criteria")?;
        criteria.push(Criterion { id, text });
    }
    Ok(criteria)
}

fn parse_evidence_requirements(
    payload: &Value,
) -> Result<Vec<EvidenceRequirement>, ProjectionError> {
    let Some(value) = payload.get("evidence_requirements") else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(invariant(ProjectionInvariant::InvalidField {
            field: "evidence_requirements",
        }));
    };
    if items.len() > MAX_EVIDENCE_REQUIREMENTS {
        return Err(invariant(ProjectionInvariant::TooManyEvidenceRequirements));
    }
    let mut requirements = Vec::with_capacity(items.len());
    for item in items {
        let criterion_id = bounded_string(item, "criterion_id", "evidence_requirements")?;
        let kinds = match item.get("kinds") {
            None => Vec::new(),
            Some(Value::Array(values)) => {
                let mut kinds = Vec::with_capacity(values.len());
                for kind in values {
                    let Some(text) = kind.as_str() else {
                        return Err(invariant(ProjectionInvariant::InvalidField {
                            field: "evidence_requirements",
                        }));
                    };
                    if text.len() > MAX_CRITERION_TEXT_BYTES {
                        return Err(invariant(ProjectionInvariant::FieldTooLong {
                            field: "evidence_requirements",
                        }));
                    }
                    kinds.push(text.to_owned());
                }
                kinds
            }
            Some(_) => {
                return Err(invariant(ProjectionInvariant::InvalidField {
                    field: "evidence_requirements",
                }));
            }
        };
        requirements.push(EvidenceRequirement {
            criterion_id,
            kinds,
        });
    }
    Ok(requirements)
}

fn parse_budget(payload: &Value) -> Result<GoalBudget, ProjectionError> {
    let source = payload.get("budget").unwrap_or(payload);
    Ok(GoalBudget {
        max_turns: optional_u64(source, "max_turns")?,
        max_tokens: optional_u64(source, "max_tokens")?,
    })
}

fn parse_usage(payload: &Value) -> Result<GoalUsage, ProjectionError> {
    let source = payload.get("usage").unwrap_or(payload);
    Ok(GoalUsage {
        turns: optional_u64(source, "turns")?.unwrap_or(0),
        tokens: optional_u64(source, "tokens")?.unwrap_or(0),
    })
}

fn optional_u64(source: &Value, field: &'static str) -> Result<Option<u64>, ProjectionError> {
    match source.get(field) {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invariant(ProjectionInvariant::InvalidField { field }))
            .map(Some),
    }
}

fn bounded_string(
    item: &Value,
    field: &'static str,
    parent: &'static str,
) -> Result<String, ProjectionError> {
    let Some(raw) = item.get(field).and_then(Value::as_str) else {
        return Err(invariant(ProjectionInvariant::InvalidField {
            field: parent,
        }));
    };
    if raw.len() > MAX_CRITERION_TEXT_BYTES {
        return Err(invariant(ProjectionInvariant::FieldTooLong {
            field: parent,
        }));
    }
    Ok(raw.to_owned())
}

fn payload_flag(payload: &Value, field: &str) -> bool {
    payload.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ProjectionError> {
    if cancel.is_cancelled() {
        Err(ProjectionError::Cancelled)
    } else {
        Ok(())
    }
}

fn invariant(kind: ProjectionInvariant) -> ProjectionError {
    ProjectionError::Invariant(kind)
}

impl SessionSnapshot {
    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn status(&self) -> SessionStatus {
        self.status
    }

    pub fn active_turn(&self) -> Option<TurnId> {
        self.active_turn
    }

    pub fn top_level_goal(&self) -> Option<&GoalSnapshot> {
        self.top_level_goal.as_ref()
    }

    pub fn active_agents(&self) -> &[AgentId] {
        &self.active_agents
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }
}

impl GoalSnapshot {
    pub fn id(&self) -> GoalId {
        self.id
    }

    pub fn statement(&self) -> &str {
        &self.statement
    }

    pub fn completion_criteria(&self) -> &[Criterion] {
        &self.completion_criteria
    }

    pub fn state(&self) -> GoalState {
        self.state
    }

    pub fn stop_reason(&self) -> Option<GoalStopReason> {
        self.stop_reason
    }

    pub fn budget(&self) -> &GoalBudget {
        &self.budget
    }

    pub fn usage(&self) -> &GoalUsage {
        &self.usage
    }

    pub fn evidence_requirements(&self) -> &[EvidenceRequirement] {
        &self.evidence_requirements
    }
}

impl Criterion {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl GoalBudget {
    pub fn max_turns(&self) -> Option<u64> {
        self.max_turns
    }

    pub fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }
}

impl GoalUsage {
    pub fn turns(&self) -> u64 {
        self.turns
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
}

impl EvidenceRequirement {
    pub fn criterion_id(&self) -> &str {
        &self.criterion_id
    }

    pub fn kinds(&self) -> &[String] {
        &self.kinds
    }
}

impl SessionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Paused => "paused",
            Self::Recovering => "recovering",
            Self::Closed => "closed",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for SessionStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SessionStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "ready" => Ok(Self::Ready),
            "busy" => Ok(Self::Busy),
            "paused" => Ok(Self::Paused),
            "recovering" => Ok(Self::Recovering),
            "closed" => Ok(Self::Closed),
            other => Err(de::Error::unknown_variant(
                other,
                &["ready", "busy", "paused", "recovering", "closed"],
            )),
        }
    }
}

impl GoalState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
        }
    }
}

impl fmt::Display for GoalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for GoalState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for GoalState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "blocked" => Ok(Self::Blocked),
            other => Err(de::Error::unknown_variant(
                other,
                &["active", "paused", "blocked"],
            )),
        }
    }
}

impl GoalStopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ProcessRecovered => "process_recovered",
        }
    }
}

impl fmt::Display for GoalStopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for GoalStopReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for GoalStopReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "budget_exhausted" => Ok(Self::BudgetExhausted),
            "process_recovered" => Ok(Self::ProcessRecovered),
            other => Err(de::Error::unknown_variant(
                other,
                &[
                    "completed",
                    "cancelled",
                    "budget_exhausted",
                    "process_recovered",
                ],
            )),
        }
    }
}

impl Serialize for SessionSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SessionSnapshot", 10)?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("project_id", &self.project_id)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("active_turn", &self.active_turn)?;
        state.serialize_field("top_level_goal", &self.top_level_goal)?;
        state.serialize_field("active_agents", &self.active_agents)?;
        state.serialize_field("seq", &self.seq)?;
        state.serialize_field("created_at", &self.created_at)?;
        state.serialize_field("updated_at", &self.updated_at)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SessionSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("SessionSnapshot", SNAPSHOT_FIELDS, SnapshotVisitor)
    }
}

struct SnapshotVisitor;

#[derive(Clone, Copy)]
enum SnapshotField {
    Schema,
    Id,
    ProjectId,
    Status,
    ActiveTurn,
    TopLevelGoal,
    ActiveAgents,
    Seq,
    CreatedAt,
    UpdatedAt,
}

impl SnapshotField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "schema" => Some(Self::Schema),
            "id" => Some(Self::Id),
            "project_id" => Some(Self::ProjectId),
            "status" => Some(Self::Status),
            "active_turn" => Some(Self::ActiveTurn),
            "top_level_goal" => Some(Self::TopLevelGoal),
            "active_agents" => Some(Self::ActiveAgents),
            "seq" => Some(Self::Seq),
            "created_at" => Some(Self::CreatedAt),
            "updated_at" => Some(Self::UpdatedAt),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for SnapshotField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(SnapshotFieldVisitor)
    }
}

struct SnapshotFieldVisitor;

impl Visitor<'_> for SnapshotFieldVisitor {
    type Value = SnapshotField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a SessionSnapshot field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        SnapshotField::from_str(value).ok_or_else(|| E::unknown_field(value, SNAPSHOT_FIELDS))
    }
}

impl<'de> Visitor<'de> for SnapshotVisitor {
    type Value = SessionSnapshot;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a SessionSnapshot object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut schema = None;
        let mut id = None;
        let mut project_id = None;
        let mut status = None;
        let mut active_turn = None;
        let mut top_level_goal = None;
        let mut active_agents = None;
        let mut seq = None;
        let mut created_at = None;
        let mut updated_at = None;

        while let Some(field) = access.next_key()? {
            match field {
                SnapshotField::Schema => assign_once(&mut schema, access.next_value()?, "schema")?,
                SnapshotField::Id => assign_once(&mut id, access.next_value()?, "id")?,
                SnapshotField::ProjectId => {
                    assign_once(&mut project_id, access.next_value()?, "project_id")?;
                }
                SnapshotField::Status => assign_once(&mut status, access.next_value()?, "status")?,
                SnapshotField::ActiveTurn => {
                    assign_once(&mut active_turn, access.next_value()?, "active_turn")?;
                }
                SnapshotField::TopLevelGoal => {
                    assign_once(&mut top_level_goal, access.next_value()?, "top_level_goal")?;
                }
                SnapshotField::ActiveAgents => {
                    assign_once(&mut active_agents, access.next_value()?, "active_agents")?;
                }
                SnapshotField::Seq => assign_once(&mut seq, access.next_value()?, "seq")?,
                SnapshotField::CreatedAt => {
                    assign_once(&mut created_at, access.next_value()?, "created_at")?;
                }
                SnapshotField::UpdatedAt => {
                    assign_once(&mut updated_at, access.next_value()?, "updated_at")?;
                }
            }
        }

        let schema: u16 = schema.ok_or_else(|| de::Error::missing_field("schema"))?;
        if schema != SESSION_SNAPSHOT_SCHEMA {
            return Err(de::Error::custom(format!(
                "unsupported session snapshot schema {schema} (expected {SESSION_SNAPSHOT_SCHEMA})"
            )));
        }

        Ok(SessionSnapshot {
            schema,
            id: id.ok_or_else(|| de::Error::missing_field("id"))?,
            project_id: project_id.ok_or_else(|| de::Error::missing_field("project_id"))?,
            status: status.ok_or_else(|| de::Error::missing_field("status"))?,
            active_turn: active_turn.unwrap_or(None),
            top_level_goal: top_level_goal.unwrap_or(None),
            active_agents: active_agents
                .ok_or_else(|| de::Error::missing_field("active_agents"))?,
            seq: seq.ok_or_else(|| de::Error::missing_field("seq"))?,
            created_at: created_at.ok_or_else(|| de::Error::missing_field("created_at"))?,
            updated_at: updated_at.ok_or_else(|| de::Error::missing_field("updated_at"))?,
        })
    }
}

fn assign_once<T, E: de::Error>(
    slot: &mut Option<T>,
    value: T,
    field: &'static str,
) -> Result<(), E> {
    if slot.is_some() {
        Err(E::duplicate_field(field))
    } else {
        *slot = Some(value);
        Ok(())
    }
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("session projection cancelled"),
            Self::TooManyEvents => f.write_str("session projection exceeds the event bound"),
            Self::Invariant(inner) => inner.fmt(f),
        }
    }
}

impl Error for ProjectionError {}

impl fmt::Display for ProjectionInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionNotCreated => {
                f.write_str("session projection has no session.created event")
            }
            Self::SessionAlreadyCreated => f.write_str("session is already created"),
            Self::SessionClosed => f.write_str("session is closed"),
            Self::SessionIdMismatch { expected, found } => {
                write!(
                    f,
                    "event session {found} does not match projection {expected}"
                )
            }
            Self::SeqGap { expected, found } => {
                write!(f, "event seq {found} is not the next expected {expected}")
            }
            Self::MissingField { field } => write!(f, "event payload missing field {field}"),
            Self::InvalidField { field } => write!(f, "event payload has invalid field {field}"),
            Self::FieldTooLong { field } => write!(f, "event payload field {field} exceeds bound"),
            Self::TooManyCriteria => f.write_str("goal completion_criteria exceeds bound"),
            Self::TooManyEvidenceRequirements => {
                f.write_str("goal evidence_requirements exceeds bound")
            }
            Self::TurnAlreadyActive => f.write_str("session already has an active turn"),
            Self::TurnNotActive => f.write_str("session has no active turn"),
            Self::TurnMismatch { expected, found } => {
                write!(
                    f,
                    "event turn {found} does not match active turn {expected}"
                )
            }
            Self::GoalAlreadyActive => f.write_str("session already has a top-level goal"),
            Self::GoalNotActive => f.write_str("session has no top-level goal"),
            Self::GoalMismatch { expected, found } => {
                write!(
                    f,
                    "event goal {found} does not match top-level goal {expected}"
                )
            }
            Self::GoalInvalidTransition { from, kind } => {
                write!(f, "goal state {from} cannot apply {}", kind.as_str())
            }
            Self::AgentAlreadyActive => f.write_str("agent is already active on the session"),
            Self::AgentNotActive => f.write_str("agent is not active on the session"),
            Self::AgentLimit => f.write_str("session active-agent bound exceeded"),
        }
    }
}

impl Error for ProjectionInvariant {}

#[cfg(test)]
mod tests {
    use super::*;
    use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, RecordedAt};
    use protocol::{EventId, RedactionClass, TraceId};

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
    const PROJECT_ID: &str = "019c0000-0000-7000-8000-000000000011";
    const TURN_ID: &str = "019c0000-0000-7000-8000-000000000012";
    const TURN_ID_B: &str = "019c0000-0000-7000-8000-000000000013";
    const GOAL_ID: &str = "019c0000-0000-7000-8000-000000000014";
    const AGENT_ID: &str = "019c0000-0000-7000-8000-000000000015";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000017";
    const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
    const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";

    const GOLDEN_CREATED: &str = r#"{"schema":1,"id":"019c0000-0000-7000-8000-000000000010","project_id":"019c0000-0000-7000-8000-000000000011","status":"ready","active_turn":null,"top_level_goal":null,"active_agents":[],"seq":1,"created_at":"2026-08-14T15:20:04.123Z","updated_at":"2026-08-14T15:20:04.123Z"}"#;

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

    fn event_id_for_seq(seq: u64) -> EventId {
        format!("019c0000-0000-7000-8000-{seq:012x}")
            .parse()
            .expect("event id")
    }

    fn created() -> ErasedEventEnvelope {
        envelope(
            1,
            EventKind::SessionCreated,
            CREATED_AT,
            serde_json::json!({"project_id": PROJECT_ID}),
        )
    }

    fn replay_ok(events: &[ErasedEventEnvelope]) -> SessionSnapshot {
        replay(events, &CancellationToken::new()).expect("replay")
    }

    fn apply_ok(events: &[ErasedEventEnvelope]) -> SessionSnapshot {
        let mut snapshot = None;
        for event in events {
            snapshot = Some(apply(snapshot, event).expect("apply"));
        }
        snapshot.expect("snapshot")
    }

    #[test]
    fn created_session_matches_golden() {
        let snapshot = replay_ok(&[created()]);
        let json = serde_json::to_string(&snapshot).expect("serialize");
        assert_eq!(json, GOLDEN_CREATED);
        let decoded: SessionSnapshot = serde_json::from_str(GOLDEN_CREATED).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(snapshot.status(), SessionStatus::Ready);
        assert_eq!(snapshot.seq(), 1);
        assert_eq!(snapshot.created_at(), CREATED_AT);
    }

    #[test]
    fn replay_same_list_is_byte_equal() {
        let events = vec![
            created(),
            envelope(
                2,
                EventKind::GoalCreated,
                UPDATED_AT,
                serde_json::json!({
                    "goal_id": GOAL_ID,
                    "statement": "ship the projection",
                    "completion_criteria": [{"id": "c1", "text": "tests pass"}],
                    "budget": {"max_turns": 8, "max_tokens": 1000},
                    "future_flag": true
                }),
            ),
            envelope(
                3,
                EventKind::TurnStarted,
                UPDATED_AT,
                serde_json::json!({"turn_id": TURN_ID, "ignored": 1}),
            ),
            envelope(
                4,
                EventKind::AgentSpawned,
                UPDATED_AT,
                serde_json::json!({"agent_id": AGENT_ID}),
            ),
            envelope(
                5,
                EventKind::ModelStreamDelta,
                UPDATED_AT,
                serde_json::json!({"chunk": "hi"}),
            ),
            envelope(
                6,
                EventKind::TurnCompleted,
                UPDATED_AT,
                serde_json::json!({"turn_id": TURN_ID}),
            ),
            envelope(
                7,
                EventKind::GoalPaused,
                UPDATED_AT,
                serde_json::json!({"goal_id": GOAL_ID}),
            ),
        ];

        let first = replay_ok(&events);
        let second = replay_ok(&events);
        let folded = apply_ok(&events);
        let a = serde_json::to_vec(&first).expect("bytes a");
        let b = serde_json::to_vec(&second).expect("bytes b");
        let c = serde_json::to_vec(&folded).expect("bytes c");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(first, second);
        assert_eq!(first.status(), SessionStatus::Paused);
        assert_eq!(first.active_agents().len(), 1);
        assert_eq!(
            first.top_level_goal().expect("goal").state(),
            GoalState::Paused
        );
        assert_eq!(first.updated_at(), UPDATED_AT);
    }

    #[test]
    fn forked_session_starts_ready() {
        let event = envelope(
            1,
            EventKind::SessionForked,
            CREATED_AT,
            serde_json::json!({"project_id": PROJECT_ID, "parent_session_id": SESSION_ID}),
        );
        let snapshot = replay_ok(&[event]);
        assert_eq!(snapshot.status(), SessionStatus::Ready);
        assert!(snapshot.top_level_goal().is_none());
    }

    #[test]
    fn recovered_inflight_stays_recovering_until_resolved() {
        let events = vec![
            created(),
            envelope(
                2,
                EventKind::GoalCreated,
                UPDATED_AT,
                serde_json::json!({"goal_id": GOAL_ID, "statement": "work"}),
            ),
            envelope(
                3,
                EventKind::TurnStarted,
                UPDATED_AT,
                serde_json::json!({"turn_id": TURN_ID}),
            ),
            envelope(
                4,
                EventKind::SessionRecovered,
                UPDATED_AT,
                serde_json::json!({}),
            ),
            envelope(
                5,
                EventKind::ModelStreamDelta,
                UPDATED_AT,
                serde_json::json!({}),
            ),
        ];
        let snapshot = replay_ok(&events);
        assert_eq!(snapshot.status(), SessionStatus::Recovering);
        assert_eq!(snapshot.active_turn().unwrap().to_string(), TURN_ID);

        let mut resolved = events;
        resolved.push(envelope(
            6,
            EventKind::TurnInterrupted,
            UPDATED_AT,
            serde_json::json!({"turn_id": TURN_ID}),
        ));
        resolved.push(envelope(
            7,
            EventKind::GoalPaused,
            UPDATED_AT,
            serde_json::json!({"goal_id": GOAL_ID, "process_recovered": true}),
        ));
        let parked = replay_ok(&resolved);
        assert_eq!(parked.status(), SessionStatus::Paused);
        assert!(parked.active_turn().is_none());
        assert_eq!(
            parked.top_level_goal().expect("goal").stop_reason(),
            Some(GoalStopReason::ProcessRecovered)
        );
    }

    #[test]
    fn invalid_transitions_return_invariant_errors() {
        let ready = replay_ok(&[created()]);

        let before_create = envelope(
            1,
            EventKind::TurnStarted,
            CREATED_AT,
            serde_json::json!({"turn_id": TURN_ID}),
        );
        assert!(matches!(
            apply(None, &before_create),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::SessionNotCreated
            ))
        ));

        let second_create = envelope(
            2,
            EventKind::SessionCreated,
            UPDATED_AT,
            serde_json::json!({"project_id": PROJECT_ID}),
        );
        assert!(matches!(
            apply(Some(ready.clone()), &second_create),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::SessionAlreadyCreated
            ))
        ));

        let gap = envelope(
            3,
            EventKind::TurnStarted,
            UPDATED_AT,
            serde_json::json!({"turn_id": TURN_ID}),
        );
        assert!(matches!(
            apply(Some(ready.clone()), &gap),
            Err(ProjectionError::Invariant(ProjectionInvariant::SeqGap {
                expected: 2,
                found: 3
            }))
        ));

        let other_session = EventEnvelope::new(
            "019c0000-0000-7000-8000-000000000099"
                .parse()
                .expect("event"),
            "019c0000-0000-7000-8000-000000000098"
                .parse()
                .expect("session"),
            2,
            UPDATED_AT.parse().expect("at"),
            ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
            TRACE_ID.parse().expect("trace"),
            EventKind::TurnStarted,
            RedactionClass::Project,
            serde_json::json!({"turn_id": TURN_ID}),
        );
        assert!(matches!(
            apply(Some(ready.clone()), &other_session),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::SessionIdMismatch { .. }
            ))
        ));

        let started = apply_ok(&[
            created(),
            envelope(
                2,
                EventKind::TurnStarted,
                UPDATED_AT,
                serde_json::json!({"turn_id": TURN_ID}),
            ),
        ]);
        assert_eq!(started.status(), SessionStatus::Busy);
        let second_turn = envelope(
            3,
            EventKind::TurnStarted,
            UPDATED_AT,
            serde_json::json!({"turn_id": TURN_ID_B}),
        );
        assert!(matches!(
            apply(Some(started.clone()), &second_turn),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::TurnAlreadyActive
            ))
        ));

        let mismatch = envelope(
            3,
            EventKind::TurnCompleted,
            UPDATED_AT,
            serde_json::json!({"turn_id": TURN_ID_B}),
        );
        assert!(matches!(
            apply(Some(started), &mismatch),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::TurnMismatch { .. }
            ))
        ));

        let complete_idle = envelope(
            2,
            EventKind::TurnCompleted,
            UPDATED_AT,
            serde_json::json!({"turn_id": TURN_ID}),
        );
        assert!(matches!(
            apply(Some(ready.clone()), &complete_idle),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::TurnNotActive
            ))
        ));

        let closed = apply_ok(&[
            created(),
            envelope(
                2,
                EventKind::SessionClosed,
                UPDATED_AT,
                serde_json::json!({}),
            ),
        ]);
        assert_eq!(closed.status(), SessionStatus::Closed);
        let after_close = envelope(
            3,
            EventKind::TurnStarted,
            UPDATED_AT,
            serde_json::json!({"turn_id": TURN_ID}),
        );
        assert!(matches!(
            apply(Some(closed), &after_close),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::SessionClosed
            ))
        ));

        let with_goal = apply_ok(&[
            created(),
            envelope(
                2,
                EventKind::GoalCreated,
                UPDATED_AT,
                serde_json::json!({"goal_id": GOAL_ID, "statement": "one"}),
            ),
        ]);
        let second_goal = envelope(
            3,
            EventKind::GoalCreated,
            UPDATED_AT,
            serde_json::json!({"goal_id": GOAL_ID, "statement": "two"}),
        );
        assert!(matches!(
            apply(Some(with_goal.clone()), &second_goal),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::GoalAlreadyActive
            ))
        ));

        let resume_active = envelope(
            3,
            EventKind::GoalResumed,
            UPDATED_AT,
            serde_json::json!({"goal_id": GOAL_ID}),
        );
        assert!(matches!(
            apply(Some(with_goal), &resume_active),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::GoalInvalidTransition { .. }
            ))
        ));

        let pause_missing = envelope(
            2,
            EventKind::GoalPaused,
            UPDATED_AT,
            serde_json::json!({"goal_id": GOAL_ID}),
        );
        assert!(matches!(
            apply(Some(ready), &pause_missing),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::GoalNotActive
            ))
        ));
    }

    #[test]
    fn replay_empty_and_cancelled() {
        assert!(matches!(
            replay(&[], &CancellationToken::new()),
            Err(ProjectionError::Invariant(
                ProjectionInvariant::SessionNotCreated
            ))
        ));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            replay(&[created()], &cancel),
            Err(ProjectionError::Cancelled)
        );
    }

    #[test]
    fn first_event_seq_must_be_one() {
        let event = envelope(
            2,
            EventKind::SessionCreated,
            CREATED_AT,
            serde_json::json!({"project_id": PROJECT_ID}),
        );
        assert!(matches!(
            apply(None, &event),
            Err(ProjectionError::Invariant(ProjectionInvariant::SeqGap {
                expected: 1,
                found: 2
            }))
        ));
    }
}
