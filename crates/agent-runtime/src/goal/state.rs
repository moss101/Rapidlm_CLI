//! Top-level goal lifecycle. Complete/cancel are terminal events, not states.
//!
//! Only a human, system, or main-agent actor may mutate the snapshot.
//! Subagent commands fail closed. Model text cannot override these checks.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use protocol::{AgentId, ErrorCode, GoalId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::agent::model::CancellationToken;

/// Wire schema name for [`GoalSnapshot`].
pub const GOAL_SNAPSHOT_SCHEMA: &str = "rapidlm.goal_snapshot";

/// v1 schema version for goal snapshot objects.
pub const GOAL_SCHEMA_VERSION: u16 = 1;

/// Maximum UTF-8 bytes accepted in [`GoalSpec::statement`].
pub const MAX_GOAL_STATEMENT_BYTES: usize = 16 * 1024;

/// Maximum completion criteria retained on one snapshot.
pub const MAX_CRITERIA: usize = 64;

/// Maximum UTF-8 bytes accepted in one criterion identifier, text, or kind.
pub const MAX_CRITERION_TEXT_BYTES: usize = 4 * 1024;

/// Maximum evidence-requirement entries retained on one snapshot.
pub const MAX_EVIDENCE_REQUIREMENTS: usize = 64;

/// Maximum evidence kinds accepted on one requirement.
pub const MAX_EVIDENCE_KINDS: usize = 16;

const SNAPSHOT_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "statement",
    "completion_criteria",
    "state",
    "stop_reason",
    "budget",
    "usage",
    "evidence_requirements",
];

const BUDGET_FIELDS: &[&str] = &["max_turns", "max_tokens", "max_active_ms", "max_cost"];

const USAGE_FIELDS: &[&str] = &["turns", "tokens", "active_ms", "cost"];

const CRITERION_FIELDS: &[&str] = &["id", "text"];

const EVIDENCE_REQUIREMENT_FIELDS: &[&str] = &["criterion_id", "kinds"];

/// Durable top-level goal lifecycle. Completion/cancel clear the snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum GoalState {
    Active,
    Paused,
    Blocked,
}

/// Why a still-projected goal stopped making progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum GoalStopReason {
    Completed,
    Cancelled,
    BudgetExhausted,
    ProcessRecovered,
}

/// Ledger event emitted by a successful [`GoalStateMachine::apply`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum GoalEventKind {
    Created,
    Replaced,
    Paused,
    Resumed,
    Blocked,
    Completed,
    Cancelled,
}

/// Discriminant of [`GoalCommand`] for typed errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum GoalCommandKind {
    Create,
    Replace,
    Pause,
    Resume,
    Block,
    Complete,
    Cancel,
}

/// Actor that requested a top-level goal mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GoalActor {
    Human,
    System,
    MainAgent { agent_id: AgentId },
    Subagent { agent_id: AgentId },
}

/// Create/replace/pause/resume/block/complete/cancel. Field edits are not a
/// lifecycle edge; a new contract is atomically substituted via `Replace`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommand {
    Create(GoalSpec),
    Replace(GoalSpec),
    Pause {
        goal_id: GoalId,
        process_recovered: bool,
    },
    Resume {
        goal_id: GoalId,
    },
    Block {
        goal_id: GoalId,
        budget_exhausted: bool,
    },
    Complete {
        goal_id: GoalId,
    },
    Cancel {
        goal_id: GoalId,
    },
}

/// Validated create payload. Usage starts at zero and state is `active`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalSpec {
    id: GoalId,
    statement: String,
    completion_criteria: Vec<Criterion>,
    budget: GoalBudget,
    evidence_requirements: Vec<EvidenceRequirement>,
}

/// One completion criterion copied onto the snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Criterion {
    id: String,
    text: String,
}

/// Configured goal ceilings. Absent fields are unbounded; none are invented.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct GoalBudget {
    max_turns: Option<u64>,
    max_tokens: Option<u64>,
    max_active_ms: Option<u64>,
    max_cost: Option<u64>,
}

/// Observed consumption. Independent of ceilings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct GoalUsage {
    turns: u64,
    tokens: u64,
    active_ms: u64,
    cost: u64,
}

/// Evidence expected before a criterion may pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceRequirement {
    criterion_id: String,
    kinds: Vec<String>,
}

/// At most one top-level goal snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
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

/// Result of a legal apply: the ledger event and the post-apply snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalEffect {
    event: GoalEventKind,
    goal_id: GoalId,
    snapshot: Option<GoalSnapshot>,
    invalidate_context: bool,
}

/// Session-scoped machine. Holds zero or one top-level snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GoalStateMachine {
    snapshot: Option<GoalSnapshot>,
}

/// Typed lifecycle failure. Display never echoes goal statement text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalStateError {
    Cancelled,
    UnauthorizedActor,
    AlreadyActive {
        existing: GoalId,
    },
    NotActive,
    GoalMismatch {
        expected: GoalId,
        found: GoalId,
    },
    InvalidTransition {
        from: Option<GoalState>,
        command: GoalCommandKind,
    },
    InvalidStatement,
    InvalidCriterion,
    TooManyCriteria {
        limit: usize,
    },
    TooManyEvidenceRequirements {
        limit: usize,
    },
    TooManyEvidenceKinds {
        limit: usize,
    },
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

impl GoalActor {
    /// Main agent, human, and system may mutate. Subagents may not.
    pub const fn can_mutate_top_level(self) -> bool {
        !matches!(self, Self::Subagent { .. })
    }

    pub const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Human | Self::System => None,
            Self::MainAgent { agent_id } | Self::Subagent { agent_id } => Some(agent_id),
        }
    }
}

impl GoalCommand {
    pub const fn kind(&self) -> GoalCommandKind {
        match self {
            Self::Create(_) => GoalCommandKind::Create,
            Self::Replace(_) => GoalCommandKind::Replace,
            Self::Pause { .. } => GoalCommandKind::Pause,
            Self::Resume { .. } => GoalCommandKind::Resume,
            Self::Block { .. } => GoalCommandKind::Block,
            Self::Complete { .. } => GoalCommandKind::Complete,
            Self::Cancel { .. } => GoalCommandKind::Cancel,
        }
    }

    pub const fn goal_id(&self) -> Option<GoalId> {
        match self {
            Self::Create(spec) | Self::Replace(spec) => Some(spec.id),
            Self::Pause { goal_id, .. }
            | Self::Resume { goal_id }
            | Self::Block { goal_id, .. }
            | Self::Complete { goal_id }
            | Self::Cancel { goal_id } => Some(*goal_id),
        }
    }
}

impl GoalSpec {
    pub fn new(
        id: GoalId,
        statement: impl Into<String>,
        completion_criteria: Vec<Criterion>,
        budget: GoalBudget,
        evidence_requirements: Vec<EvidenceRequirement>,
    ) -> Result<Self, GoalStateError> {
        let statement = statement.into();
        validate_statement(&statement)?;
        if completion_criteria.len() > MAX_CRITERIA {
            return Err(GoalStateError::TooManyCriteria {
                limit: MAX_CRITERIA,
            });
        }
        if evidence_requirements.len() > MAX_EVIDENCE_REQUIREMENTS {
            return Err(GoalStateError::TooManyEvidenceRequirements {
                limit: MAX_EVIDENCE_REQUIREMENTS,
            });
        }
        Ok(Self {
            id,
            statement,
            completion_criteria,
            budget,
            evidence_requirements,
        })
    }

    pub fn id(&self) -> GoalId {
        self.id
    }

    pub fn statement(&self) -> &str {
        &self.statement
    }

    pub fn completion_criteria(&self) -> &[Criterion] {
        &self.completion_criteria
    }

    pub fn budget(&self) -> GoalBudget {
        self.budget
    }

    pub fn evidence_requirements(&self) -> &[EvidenceRequirement] {
        &self.evidence_requirements
    }
}

impl Criterion {
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Result<Self, GoalStateError> {
        let id = id.into();
        let text = text.into();
        if !valid_bounded_text(&id) || !valid_bounded_text(&text) {
            return Err(GoalStateError::InvalidCriterion);
        }
        Ok(Self { id, text })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl EvidenceRequirement {
    pub fn new(
        criterion_id: impl Into<String>,
        kinds: Vec<String>,
    ) -> Result<Self, GoalStateError> {
        let criterion_id = criterion_id.into();
        if !valid_bounded_text(&criterion_id) {
            return Err(GoalStateError::InvalidCriterion);
        }
        if kinds.len() > MAX_EVIDENCE_KINDS {
            return Err(GoalStateError::TooManyEvidenceKinds {
                limit: MAX_EVIDENCE_KINDS,
            });
        }
        for kind in &kinds {
            if !valid_bounded_text(kind) {
                return Err(GoalStateError::InvalidCriterion);
            }
        }
        Ok(Self {
            criterion_id,
            kinds,
        })
    }

    pub fn criterion_id(&self) -> &str {
        &self.criterion_id
    }

    pub fn kinds(&self) -> &[String] {
        &self.kinds
    }
}

impl GoalBudget {
    pub const fn new(
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
        max_active_ms: Option<u64>,
        max_cost: Option<u64>,
    ) -> Self {
        Self {
            max_turns,
            max_tokens,
            max_active_ms,
            max_cost,
        }
    }

    pub const fn max_turns(self) -> Option<u64> {
        self.max_turns
    }

    pub const fn max_tokens(self) -> Option<u64> {
        self.max_tokens
    }

    pub const fn max_active_ms(self) -> Option<u64> {
        self.max_active_ms
    }

    pub const fn max_cost(self) -> Option<u64> {
        self.max_cost
    }
}

impl GoalUsage {
    pub const fn new(turns: u64, tokens: u64, active_ms: u64, cost: u64) -> Self {
        Self {
            turns,
            tokens,
            active_ms,
            cost,
        }
    }

    pub const fn turns(self) -> u64 {
        self.turns
    }

    pub const fn tokens(self) -> u64 {
        self.tokens
    }

    pub const fn active_ms(self) -> u64 {
        self.active_ms
    }

    pub const fn cost(self) -> u64 {
        self.cost
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

    pub fn budget(&self) -> GoalBudget {
        self.budget
    }

    pub fn usage(&self) -> GoalUsage {
        self.usage
    }

    pub fn evidence_requirements(&self) -> &[EvidenceRequirement] {
        &self.evidence_requirements
    }

    /// Replace observed usage. Ceilings stay on [`Self::budget`].
    pub fn with_usage(self, usage: GoalUsage) -> Self {
        Self { usage, ..self }
    }
}

impl GoalEffect {
    pub const fn event(&self) -> GoalEventKind {
        self.event
    }

    pub const fn goal_id(&self) -> GoalId {
        self.goal_id
    }

    pub fn snapshot(&self) -> Option<&GoalSnapshot> {
        self.snapshot.as_ref()
    }

    /// Cancel (and complete) drop the snapshot so prior reminders are invalid.
    pub const fn invalidate_context(&self) -> bool {
        self.invalidate_context
    }
}

impl GoalStateMachine {
    pub const fn new() -> Self {
        Self { snapshot: None }
    }

    pub const fn from_snapshot(snapshot: GoalSnapshot) -> Self {
        Self {
            snapshot: Some(snapshot),
        }
    }

    pub fn snapshot(&self) -> Option<&GoalSnapshot> {
        self.snapshot.as_ref()
    }

    /// Apply a lifecycle command. Subagent actors are rejected before any edge.
    pub fn apply(
        &mut self,
        command: GoalCommand,
        actor: &GoalActor,
    ) -> Result<GoalEffect, GoalStateError> {
        self.apply_with_cancel(command, actor, &CancellationToken::new())
    }

    pub fn apply_with_cancel(
        &mut self,
        command: GoalCommand,
        actor: &GoalActor,
        cancel: &CancellationToken,
    ) -> Result<GoalEffect, GoalStateError> {
        if cancel.is_cancelled() {
            return Err(GoalStateError::Cancelled);
        }
        if !actor.can_mutate_top_level() {
            return Err(GoalStateError::UnauthorizedActor);
        }
        match command {
            GoalCommand::Create(spec) => self.create(spec),
            GoalCommand::Replace(spec) => self.replace(spec),
            GoalCommand::Pause {
                goal_id,
                process_recovered,
            } => self.set_state(
                goal_id,
                GoalState::Paused,
                GoalEventKind::Paused,
                GoalCommandKind::Pause,
                process_recovered.then_some(GoalStopReason::ProcessRecovered),
            ),
            GoalCommand::Resume { goal_id } => self.set_state(
                goal_id,
                GoalState::Active,
                GoalEventKind::Resumed,
                GoalCommandKind::Resume,
                None,
            ),
            GoalCommand::Block {
                goal_id,
                budget_exhausted,
            } => self.set_state(
                goal_id,
                GoalState::Blocked,
                GoalEventKind::Blocked,
                GoalCommandKind::Block,
                budget_exhausted.then_some(GoalStopReason::BudgetExhausted),
            ),
            GoalCommand::Complete { goal_id } => self.clear(goal_id, GoalEventKind::Completed),
            GoalCommand::Cancel { goal_id } => self.clear(goal_id, GoalEventKind::Cancelled),
        }
    }

    fn create(&mut self, spec: GoalSpec) -> Result<GoalEffect, GoalStateError> {
        if let Some(existing) = &self.snapshot {
            return Err(GoalStateError::AlreadyActive {
                existing: existing.id,
            });
        }
        let snapshot = GoalSnapshot {
            id: spec.id,
            statement: spec.statement,
            completion_criteria: spec.completion_criteria,
            state: GoalState::Active,
            stop_reason: None,
            budget: spec.budget,
            usage: GoalUsage::default(),
            evidence_requirements: spec.evidence_requirements,
        };
        self.snapshot = Some(snapshot.clone());
        Ok(GoalEffect {
            event: GoalEventKind::Created,
            goal_id: snapshot.id,
            snapshot: Some(snapshot),
            invalidate_context: false,
        })
    }

    /// Atomically substitute the active goal contract with a new `spec`. Usage
    /// resets to zero (a fresh contract), state returns to `active`, and the
    /// prior snapshot is invalidated so stale continuation prompts are dropped.
    fn replace(&mut self, spec: GoalSpec) -> Result<GoalEffect, GoalStateError> {
        if self.snapshot.is_none() {
            return Err(GoalStateError::NotActive);
        }
        let snapshot = GoalSnapshot {
            id: spec.id,
            statement: spec.statement,
            completion_criteria: spec.completion_criteria,
            state: GoalState::Active,
            stop_reason: None,
            budget: spec.budget,
            usage: GoalUsage::default(),
            evidence_requirements: spec.evidence_requirements,
        };
        self.snapshot = Some(snapshot.clone());
        Ok(GoalEffect {
            event: GoalEventKind::Replaced,
            goal_id: snapshot.id,
            snapshot: Some(snapshot),
            invalidate_context: true,
        })
    }

    fn set_state(
        &mut self,
        goal_id: GoalId,
        next: GoalState,
        event: GoalEventKind,
        command: GoalCommandKind,
        stop_reason: Option<GoalStopReason>,
    ) -> Result<GoalEffect, GoalStateError> {
        let goal = self.require_goal(goal_id)?;
        if goal.state == next {
            return Err(GoalStateError::InvalidTransition {
                from: Some(goal.state),
                command,
            });
        }
        goal.state = next;
        goal.stop_reason = match next {
            GoalState::Active => None,
            GoalState::Paused | GoalState::Blocked => stop_reason,
        };
        let snapshot = goal.clone();
        Ok(GoalEffect {
            event,
            goal_id: snapshot.id,
            snapshot: Some(snapshot),
            invalidate_context: false,
        })
    }

    fn clear(
        &mut self,
        goal_id: GoalId,
        event: GoalEventKind,
    ) -> Result<GoalEffect, GoalStateError> {
        let _ = self.require_goal(goal_id)?;
        self.snapshot = None;
        Ok(GoalEffect {
            event,
            goal_id,
            snapshot: None,
            invalidate_context: true,
        })
    }

    fn require_goal(&mut self, goal_id: GoalId) -> Result<&mut GoalSnapshot, GoalStateError> {
        let Some(goal) = self.snapshot.as_mut() else {
            return Err(GoalStateError::NotActive);
        };
        if goal.id != goal_id {
            return Err(GoalStateError::GoalMismatch {
                expected: goal.id,
                found: goal_id,
            });
        }
        Ok(goal)
    }
}

impl GoalState {
    pub const ALL: &'static [Self] = &[Self::Active, Self::Paused, Self::Blocked];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
        }
    }
}

impl GoalStopReason {
    pub const ALL: &'static [Self] = &[
        Self::Completed,
        Self::Cancelled,
        Self::BudgetExhausted,
        Self::ProcessRecovered,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ProcessRecovered => "process_recovered",
        }
    }
}

impl GoalEventKind {
    pub const ALL: &'static [Self] = &[
        Self::Created,
        Self::Replaced,
        Self::Paused,
        Self::Resumed,
        Self::Blocked,
        Self::Completed,
        Self::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "goal.created",
            Self::Replaced => "goal.replaced",
            Self::Paused => "goal.paused",
            Self::Resumed => "goal.resumed",
            Self::Blocked => "goal.blocked",
            Self::Completed => "goal.completed",
            Self::Cancelled => "goal.cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

impl GoalCommandKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Replace => "replace",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Block => "block",
            Self::Complete => "complete",
            Self::Cancel => "cancel",
        }
    }
}

impl GoalStateError {
    /// Public error code when this failure has a wire mapping.
    ///
    /// [`GoalStateError::Cancelled`] has no public code.
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::UnauthorizedActor => Some(ErrorCode::PolicyDenied),
            Self::AlreadyActive { .. }
            | Self::NotActive
            | Self::GoalMismatch { .. }
            | Self::InvalidTransition { .. } => Some(ErrorCode::GoalInvalidTransition),
            Self::InvalidStatement
            | Self::InvalidCriterion
            | Self::TooManyCriteria { .. }
            | Self::TooManyEvidenceRequirements { .. }
            | Self::TooManyEvidenceKinds { .. }
            | Self::UnsupportedSchema
            | Self::UnsupportedSchemaVersion => Some(ErrorCode::ConfigInvalid),
        }
    }
}

fn validate_statement(statement: &str) -> Result<(), GoalStateError> {
    if statement.is_empty() || statement.len() > MAX_GOAL_STATEMENT_BYTES {
        return Err(GoalStateError::InvalidStatement);
    }
    Ok(())
}

fn valid_bounded_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_CRITERION_TEXT_BYTES
}

impl fmt::Display for GoalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for GoalStopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for GoalEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for GoalCommandKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for GoalStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("goal mutation cancelled"),
            Self::UnauthorizedActor => {
                f.write_str("subagent cannot mutate top-level goal lifecycle")
            }
            Self::AlreadyActive { existing } => {
                write!(f, "session already has top-level goal {existing}")
            }
            Self::NotActive => f.write_str("session has no top-level goal"),
            Self::GoalMismatch { expected, found } => {
                write!(
                    f,
                    "command goal {found} does not match top-level goal {expected}"
                )
            }
            Self::InvalidTransition { from, command } => match from {
                Some(state) => write!(f, "goal state {state} cannot apply {command}"),
                None => write!(f, "no top-level goal cannot apply {command}"),
            },
            Self::InvalidStatement => f.write_str("goal statement is empty or exceeds bound"),
            Self::InvalidCriterion => f.write_str("goal criterion or evidence kind is invalid"),
            Self::TooManyCriteria { limit } => {
                write!(f, "goal completion_criteria exceeds {limit}")
            }
            Self::TooManyEvidenceRequirements { limit } => {
                write!(f, "goal evidence_requirements exceeds {limit}")
            }
            Self::TooManyEvidenceKinds { limit } => {
                write!(f, "goal evidence kinds exceeds {limit}")
            }
            Self::UnsupportedSchema => f.write_str("unsupported goal snapshot schema"),
            Self::UnsupportedSchemaVersion => {
                f.write_str("unsupported goal snapshot schema version")
            }
        }
    }
}

impl Error for GoalStateError {}

impl FromStr for GoalState {
    type Err = GoalStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "blocked" => Ok(Self::Blocked),
            _ => Err(GoalStateError::UnsupportedSchema),
        }
    }
}

impl FromStr for GoalStopReason {
    type Err = GoalStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "budget_exhausted" => Ok(Self::BudgetExhausted),
            "process_recovered" => Ok(Self::ProcessRecovered),
            _ => Err(GoalStateError::UnsupportedSchema),
        }
    }
}

impl FromStr for GoalEventKind {
    type Err = GoalStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "goal.created" => Ok(Self::Created),
            "goal.replaced" => Ok(Self::Replaced),
            "goal.paused" => Ok(Self::Paused),
            "goal.resumed" => Ok(Self::Resumed),
            "goal.blocked" => Ok(Self::Blocked),
            "goal.completed" => Ok(Self::Completed),
            "goal.cancelled" => Ok(Self::Cancelled),
            _ => Err(GoalStateError::UnsupportedSchema),
        }
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

impl Serialize for GoalBudget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("GoalBudget", BUDGET_FIELDS.len())?;
        state.serialize_field("max_turns", &self.max_turns)?;
        state.serialize_field("max_tokens", &self.max_tokens)?;
        state.serialize_field("max_active_ms", &self.max_active_ms)?;
        state.serialize_field("max_cost", &self.max_cost)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GoalBudget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            max_turns: Option<u64>,
            max_tokens: Option<u64>,
            max_active_ms: Option<u64>,
            max_cost: Option<u64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::new(
            raw.max_turns,
            raw.max_tokens,
            raw.max_active_ms,
            raw.max_cost,
        ))
    }
}

impl Serialize for GoalUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("GoalUsage", USAGE_FIELDS.len())?;
        state.serialize_field("turns", &self.turns)?;
        state.serialize_field("tokens", &self.tokens)?;
        state.serialize_field("active_ms", &self.active_ms)?;
        state.serialize_field("cost", &self.cost)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GoalUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            turns: u64,
            tokens: u64,
            active_ms: u64,
            cost: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::new(raw.turns, raw.tokens, raw.active_ms, raw.cost))
    }
}

impl Serialize for Criterion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Criterion", CRITERION_FIELDS.len())?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("text", &self.text)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Criterion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            id: String,
            text: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        Criterion::new(raw.id, raw.text).map_err(de::Error::custom)
    }
}

impl Serialize for EvidenceRequirement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer
            .serialize_struct("EvidenceRequirement", EVIDENCE_REQUIREMENT_FIELDS.len())?;
        state.serialize_field("criterion_id", &self.criterion_id)?;
        state.serialize_field("kinds", &self.kinds)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EvidenceRequirement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            criterion_id: String,
            kinds: Vec<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        EvidenceRequirement::new(raw.criterion_id, raw.kinds).map_err(de::Error::custom)
    }
}

impl Serialize for GoalSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("GoalSnapshot", SNAPSHOT_FIELDS.len())?;
        state.serialize_field("schema", GOAL_SNAPSHOT_SCHEMA)?;
        state.serialize_field("schema_version", &GOAL_SCHEMA_VERSION)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("statement", &self.statement)?;
        state.serialize_field("completion_criteria", &self.completion_criteria)?;
        state.serialize_field("state", &self.state)?;
        state.serialize_field("stop_reason", &self.stop_reason)?;
        state.serialize_field("budget", &self.budget)?;
        state.serialize_field("usage", &self.usage)?;
        state.serialize_field("evidence_requirements", &self.evidence_requirements)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GoalSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            id: GoalId,
            statement: String,
            completion_criteria: Vec<Criterion>,
            state: GoalState,
            stop_reason: Option<GoalStopReason>,
            budget: GoalBudget,
            usage: GoalUsage,
            evidence_requirements: Vec<EvidenceRequirement>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != GOAL_SNAPSHOT_SCHEMA {
            return Err(de::Error::custom(GoalStateError::UnsupportedSchema));
        }
        if raw.schema_version != GOAL_SCHEMA_VERSION {
            return Err(de::Error::custom(GoalStateError::UnsupportedSchemaVersion));
        }
        validate_statement(&raw.statement).map_err(de::Error::custom)?;
        if raw.completion_criteria.len() > MAX_CRITERIA {
            return Err(de::Error::custom(GoalStateError::TooManyCriteria {
                limit: MAX_CRITERIA,
            }));
        }
        if raw.evidence_requirements.len() > MAX_EVIDENCE_REQUIREMENTS {
            return Err(de::Error::custom(
                GoalStateError::TooManyEvidenceRequirements {
                    limit: MAX_EVIDENCE_REQUIREMENTS,
                },
            ));
        }
        Ok(Self {
            id: raw.id,
            statement: raw.statement,
            completion_criteria: raw.completion_criteria,
            state: raw.state,
            stop_reason: raw.stop_reason,
            budget: raw.budget,
            usage: raw.usage,
            evidence_requirements: raw.evidence_requirements,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const GOAL_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const OTHER_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
    const AGENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";

    const GOLDEN_SNAPSHOT: &str = r#"{"schema":"rapidlm.goal_snapshot","schema_version":1,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","statement":"ship auth","completion_criteria":[{"id":"c1","text":"tests pass"}],"state":"active","stop_reason":null,"budget":{"max_turns":10,"max_tokens":100000,"max_active_ms":null,"max_cost":null},"usage":{"turns":0,"tokens":0,"active_ms":0,"cost":0},"evidence_requirements":[{"criterion_id":"c1","kinds":["test"]}]}"#;

    fn parse_id<T: FromStr>(raw: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        raw.parse().expect("id")
    }

    fn goal_id() -> GoalId {
        parse_id(GOAL_ID)
    }

    fn other_id() -> GoalId {
        parse_id(OTHER_ID)
    }

    fn agent_id() -> AgentId {
        parse_id(AGENT_ID)
    }

    fn spec() -> GoalSpec {
        GoalSpec::new(
            goal_id(),
            "ship auth",
            vec![Criterion::new("c1", "tests pass").expect("criterion")],
            GoalBudget::new(Some(10), Some(100_000), None, None),
            vec![EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec")
    }

    fn machine_with_active() -> GoalStateMachine {
        let mut machine = GoalStateMachine::new();
        machine
            .apply(GoalCommand::Create(spec()), &GoalActor::Human)
            .expect("create");
        machine
    }

    #[test]
    fn snapshot_golden_round_trips() {
        let machine = machine_with_active();
        let snapshot = machine.snapshot().expect("snapshot").clone();
        let json = serde_json::to_string(&snapshot).expect("serialize");
        assert_eq!(json, GOLDEN_SNAPSHOT);
        let decoded: GoalSnapshot = serde_json::from_str(GOLDEN_SNAPSHOT).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.id(), goal_id());
        assert_eq!(decoded.state(), GoalState::Active);
        assert_eq!(decoded.budget().max_turns(), Some(10));
        assert_eq!(decoded.usage().turns(), 0);
    }

    #[test]
    fn state_wire_forms_match_domain() {
        assert_eq!(GoalState::Active.as_str(), "active");
        assert_eq!(GoalState::Paused.as_str(), "paused");
        assert_eq!(GoalState::Blocked.as_str(), "blocked");
        assert_eq!(
            "paused".parse::<GoalState>().expect("state"),
            GoalState::Paused
        );
        assert!("complete".parse::<GoalState>().is_err());
        assert_eq!(GoalEventKind::Completed.as_str(), "goal.completed");
        assert_eq!(GoalEventKind::Cancelled.as_str(), "goal.cancelled");
        assert!(GoalEventKind::Completed.is_terminal());
        assert!(!GoalEventKind::Paused.is_terminal());
    }

    #[test]
    fn create_establishes_single_active_snapshot() {
        let mut machine = GoalStateMachine::new();
        let effect = machine
            .apply(GoalCommand::Create(spec()), &GoalActor::System)
            .expect("create");
        assert_eq!(effect.event(), GoalEventKind::Created);
        assert_eq!(effect.goal_id(), goal_id());
        assert!(!effect.invalidate_context());
        let snapshot = machine.snapshot().expect("snapshot");
        assert_eq!(snapshot.state(), GoalState::Active);
        assert!(snapshot.stop_reason().is_none());
    }

    #[test]
    fn replace_substitutes_contract_resets_usage_and_invalidates() {
        // P6-003/§12: `Replace` atomically substitutes the active contract,
        // resets usage, returns to `active`, and invalidates the prior snapshot.
        let mut machine = machine_with_active();
        let fresh = GoalSpec::new(
            other_id(),
            "ship auth v2",
            vec![Criterion::new("c9", "new tests pass").expect("criterion")],
            GoalBudget::new(Some(20), Some(200_000), None, None),
            vec![EvidenceRequirement::new("c9", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec");
        let effect = machine
            .apply(GoalCommand::Replace(fresh), &GoalActor::Human)
            .expect("replace");
        assert_eq!(effect.event(), GoalEventKind::Replaced);
        assert_eq!(effect.goal_id(), other_id());
        assert!(effect.invalidate_context());
        let snapshot = machine.snapshot().expect("snapshot");
        assert_eq!(snapshot.id(), other_id());
        assert_eq!(snapshot.statement(), "ship auth v2");
        assert_eq!(snapshot.state(), GoalState::Active);
        assert_eq!(snapshot.usage().turns(), 0);
        assert_eq!(snapshot.completion_criteria().len(), 1);
        assert_eq!(snapshot.completion_criteria()[0].id(), "c9");
    }

    #[test]
    fn replace_without_active_goal_is_rejected() {
        let mut machine = GoalStateMachine::new();
        let err = machine
            .apply(GoalCommand::Replace(spec()), &GoalActor::Human)
            .expect_err("no active goal");
        assert_eq!(err, GoalStateError::NotActive);
    }

    #[test]
    fn second_create_is_rejected() {
        let mut machine = machine_with_active();
        let err = machine
            .apply(GoalCommand::Create(spec()), &GoalActor::Human)
            .expect_err("already active");
        assert_eq!(
            err,
            GoalStateError::AlreadyActive {
                existing: goal_id()
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::GoalInvalidTransition));
        assert!(machine.snapshot().is_some());
    }

    #[test]
    fn pause_resume_block_transitions() {
        let mut machine = machine_with_active();
        let paused = machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered: false,
                },
                &GoalActor::Human,
            )
            .expect("pause");
        assert_eq!(paused.event(), GoalEventKind::Paused);
        assert_eq!(machine.snapshot().expect("goal").state(), GoalState::Paused);

        let resumed = machine
            .apply(
                GoalCommand::Resume { goal_id: goal_id() },
                &GoalActor::MainAgent {
                    agent_id: agent_id(),
                },
            )
            .expect("resume");
        assert_eq!(resumed.event(), GoalEventKind::Resumed);
        assert_eq!(machine.snapshot().expect("goal").state(), GoalState::Active);

        let blocked = machine
            .apply(
                GoalCommand::Block {
                    goal_id: goal_id(),
                    budget_exhausted: true,
                },
                &GoalActor::System,
            )
            .expect("block");
        assert_eq!(blocked.event(), GoalEventKind::Blocked);
        let goal = machine.snapshot().expect("goal");
        assert_eq!(goal.state(), GoalState::Blocked);
        assert_eq!(goal.stop_reason(), Some(GoalStopReason::BudgetExhausted));
    }

    #[test]
    fn process_recovered_pause_sets_stop_reason() {
        let mut machine = machine_with_active();
        machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered: true,
                },
                &GoalActor::System,
            )
            .expect("pause");
        assert_eq!(
            machine.snapshot().expect("goal").stop_reason(),
            Some(GoalStopReason::ProcessRecovered)
        );
    }

    #[test]
    fn complete_emits_terminal_event_and_clears_snapshot() {
        let mut machine = machine_with_active();
        let effect = machine
            .apply(
                GoalCommand::Complete { goal_id: goal_id() },
                &GoalActor::MainAgent {
                    agent_id: agent_id(),
                },
            )
            .expect("complete");
        assert_eq!(effect.event(), GoalEventKind::Completed);
        assert!(effect.event().is_terminal());
        assert!(effect.snapshot().is_none());
        assert!(effect.invalidate_context());
        assert!(machine.snapshot().is_none());
    }

    #[test]
    fn cancel_clears_and_is_not_resumable() {
        let mut machine = machine_with_active();
        let effect = machine
            .apply(
                GoalCommand::Cancel { goal_id: goal_id() },
                &GoalActor::Human,
            )
            .expect("cancel");
        assert_eq!(effect.event(), GoalEventKind::Cancelled);
        assert!(machine.snapshot().is_none());

        let err = machine
            .apply(
                GoalCommand::Resume { goal_id: goal_id() },
                &GoalActor::Human,
            )
            .expect_err("not resumable");
        assert_eq!(err, GoalStateError::NotActive);
        assert_eq!(err.code(), Some(ErrorCode::GoalInvalidTransition));
    }

    #[test]
    fn create_after_complete_is_allowed() {
        let mut machine = machine_with_active();
        machine
            .apply(
                GoalCommand::Complete { goal_id: goal_id() },
                &GoalActor::Human,
            )
            .expect("complete");
        machine
            .apply(GoalCommand::Create(spec()), &GoalActor::Human)
            .expect("recreate");
        assert_eq!(machine.snapshot().expect("goal").state(), GoalState::Active);
    }

    #[test]
    fn invalid_same_state_transitions_error() {
        let mut machine = machine_with_active();
        let err = machine
            .apply(
                GoalCommand::Resume { goal_id: goal_id() },
                &GoalActor::Human,
            )
            .expect_err("already active");
        assert_eq!(
            err,
            GoalStateError::InvalidTransition {
                from: Some(GoalState::Active),
                command: GoalCommandKind::Resume
            }
        );

        machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered: false,
                },
                &GoalActor::Human,
            )
            .expect("pause");
        let err = machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered: false,
                },
                &GoalActor::Human,
            )
            .expect_err("already paused");
        assert_eq!(
            err,
            GoalStateError::InvalidTransition {
                from: Some(GoalState::Paused),
                command: GoalCommandKind::Pause
            }
        );
    }

    #[test]
    fn goal_id_mismatch_is_rejected() {
        let mut machine = machine_with_active();
        let err = machine
            .apply(
                GoalCommand::Cancel {
                    goal_id: other_id(),
                },
                &GoalActor::Human,
            )
            .expect_err("mismatch");
        assert_eq!(
            err,
            GoalStateError::GoalMismatch {
                expected: goal_id(),
                found: other_id()
            }
        );
        assert!(machine.snapshot().is_some());
    }

    #[test]
    fn subagent_mutation_is_rejected() {
        let mut machine = GoalStateMachine::new();
        let actor = GoalActor::Subagent {
            agent_id: agent_id(),
        };
        for command in [
            GoalCommand::Create(spec()),
            GoalCommand::Pause {
                goal_id: goal_id(),
                process_recovered: false,
            },
            GoalCommand::Resume { goal_id: goal_id() },
            GoalCommand::Block {
                goal_id: goal_id(),
                budget_exhausted: false,
            },
            GoalCommand::Complete { goal_id: goal_id() },
            GoalCommand::Cancel { goal_id: goal_id() },
        ] {
            let err = machine.apply(command, &actor).expect_err("subagent denied");
            assert_eq!(err, GoalStateError::UnauthorizedActor);
            assert_eq!(err.code(), Some(ErrorCode::PolicyDenied));
        }
        assert!(machine.snapshot().is_none());
    }

    #[test]
    fn cancelled_token_fails_closed() {
        let mut machine = GoalStateMachine::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = machine
            .apply_with_cancel(GoalCommand::Create(spec()), &GoalActor::Human, &cancel)
            .expect_err("cancelled");
        assert_eq!(err, GoalStateError::Cancelled);
        assert!(machine.snapshot().is_none());
    }

    #[test]
    fn empty_statement_is_rejected() {
        let err = GoalSpec::new(goal_id(), "", Vec::new(), GoalBudget::default(), Vec::new())
            .expect_err("empty");
        assert_eq!(err, GoalStateError::InvalidStatement);
        assert!(!format!("{err}").contains("ship"));
    }

    #[test]
    fn resume_clears_stop_reason() {
        let mut machine = machine_with_active();
        machine
            .apply(
                GoalCommand::Pause {
                    goal_id: goal_id(),
                    process_recovered: true,
                },
                &GoalActor::System,
            )
            .expect("pause");
        machine
            .apply(
                GoalCommand::Resume { goal_id: goal_id() },
                &GoalActor::Human,
            )
            .expect("resume");
        let goal = machine.snapshot().expect("goal");
        assert_eq!(goal.state(), GoalState::Active);
        assert!(goal.stop_reason().is_none());
    }
}
