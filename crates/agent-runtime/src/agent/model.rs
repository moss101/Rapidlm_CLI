//! Agent identity, lifecycle states, budgets, and typed results.
//!
//! `id`, `parent_id`, and `workspace_view_id` are spawn-time identity. They
//! cannot change after [`Agent::spawn`]. Terminal states cannot return to
//! `running`. Model text never overrides these checks.

use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::{
    AgentId, ArtifactId, ArtifactRef, EvidenceId, ModelPolicyName, ModelPolicyNameParseError,
    WorkspaceViewId,
};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

/// Wire schema name for [`AgentSpec`].
pub const AGENT_SPEC_SCHEMA: &str = "rapidlm.agent_spec";

/// Wire schema name for [`AgentResult`].
pub const AGENT_RESULT_SCHEMA: &str = "rapidlm.agent_result";

/// v1 schema version for agent-spec and agent-result objects.
pub const AGENT_SCHEMA_VERSION: u16 = 1;

/// Maximum UTF-8 bytes accepted in [`AgentSpec::task`].
pub const MAX_TASK_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes accepted in [`AgentResult::summary`].
pub const MAX_SUMMARY_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 bytes accepted in [`AgentSpec::permissions_profile`].
pub const MAX_PERMISSIONS_PROFILE_BYTES: usize = 256;

/// Maximum evidence IDs accepted on one [`AgentResult`].
pub const MAX_EVIDENCE: usize = 64;

/// Maximum artifact refs accepted on one [`AgentResult`].
pub const MAX_ARTIFACTS: usize = 64;

/// Maximum structured claims accepted on one [`AgentResult`].
pub const MAX_CLAIMS: usize = 64;

/// Maximum open questions accepted on one [`AgentResult`].
pub const MAX_OPEN_QUESTIONS: usize = 64;

/// Maximum blockers accepted on one [`AgentResult`].
pub const MAX_BLOCKERS: usize = 64;

/// Maximum context-lineage revisions accepted on one [`AgentResult`].
pub const MAX_LINEAGE: usize = 64;

/// Maximum distinct repair-rule keys on one [`AgentResult`].
pub const MAX_REPAIR_RULES: usize = 32;

/// Maximum UTF-8 bytes for one claim/blocker/question text.
pub const MAX_CLAIM_TEXT_BYTES: usize = 1024;

const SPEC_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "parent_id",
    "role",
    "task",
    "workspace_view_id",
    "model_policy",
    "budget",
    "permissions_profile",
];

const RESULT_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "agent_id",
    "status",
    "summary",
    "evidence",
    "workspace_view",
    "patch_summary",
    "artifacts",
    "claims",
    "open_questions",
    "blockers",
    "context_lineage",
    "tool_repair_stats",
];

const BUDGET_FIELDS: &[&str] = &["max_tokens", "max_cost", "max_active_ms", "max_tool_calls"];

const PATCH_SUMMARY_FIELDS: &[&str] = &["files_changed", "additions", "deletions"];

const STATS_FIELDS: &[&str] = &["tokens", "cost", "active_ms", "tool_calls"];

/// Spawnable agent role. Unknown wire values fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum AgentRole {
    Main,
    Planner,
    Coder,
    Explorer,
    Reviewer,
    Verifier,
    SecurityReviewer,
    ContextCurator,
    Debugger,
    Tester,
    PerformanceReviewer,
    BrowserOperator,
    ReleaseManager,
}

/// Durable lifecycle state. Wire form is snake_case.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum AgentState {
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

/// Terminal result class. Subset of [`AgentState`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum AgentTerminalStatus {
    Succeeded,
    Failed,
    Cancelled,
}

/// Identity field that cannot change after spawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IdentityField {
    Id,
    Parent,
    WorkspaceView,
    Task,
}

/// Named model-routing policy. Empty names are rejected.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct ModelPolicyRef(ModelPolicyName);

/// Token, cost, active-time, and tool-call ceilings. `None` is unbounded.
///
/// `max_cost` is USD micros so accounting stays integer. Zero is a valid
/// exhausted ceiling, not a silent unlimited fallback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct AgentBudget {
    max_tokens: Option<u64>,
    max_cost: Option<u64>,
    max_active_ms: Option<u64>,
    max_tool_calls: Option<u64>,
}

/// Observed consumption counters. Independent of ceilings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct AgentStats {
    tokens: u64,
    cost: u64,
    active_ms: u64,
    tool_calls: u64,
}

/// Compact change-set observation. No path or payload bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PatchSummary {
    files_changed: u32,
    additions: u32,
    deletions: u32,
}

/// Immutable spawn-time identity and policy. Domain `AgentSpec`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSpec {
    id: AgentId,
    parent_id: Option<AgentId>,
    role: AgentRole,
    task: String,
    workspace_view_id: WorkspaceViewId,
    model_policy: ModelPolicyRef,
    budget: AgentBudget,
    permissions_profile: String,
}

/// Outcome of one observable claim. Never private reasoning.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimResult {
    Satisfied,
    Refuted,
    Unverified,
}

impl ClaimResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::Refuted => "refuted",
            Self::Unverified => "unverified",
        }
    }
}

/// One observable assertion/result produced by the agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    criterion_id: Option<String>,
    text: String,
    result: ClaimResult,
}

/// Class of a blocker for graph/host consumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    Message,
    Tool,
    Policy,
    Evidence,
    External,
}

impl BlockerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Tool => "tool",
            Self::Policy => "policy",
            Self::Evidence => "evidence",
            Self::External => "external",
        }
    }
}

/// A structured blocker surfaced to the host/graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocker {
    kind: BlockerKind,
    summary: String,
}

/// One context provenance/revision reference, never a raw context dump.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRevision {
    source: String,
    revision: Option<ArtifactId>,
}

/// Bounded operational tool-repair statistics. No tool payloads.
#[derive(Clone, Debug, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRepairStats {
    repairs: u32,
    recovered: u32,
    rules: Vec<String>,
}

impl Claim {
    pub fn new(criterion_id: Option<String>, text: String, result: ClaimResult) -> Self {
        Self {
            criterion_id,
            text,
            result,
        }
    }

    pub fn criterion_id(&self) -> Option<&str> {
        self.criterion_id.as_deref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn result(&self) -> ClaimResult {
        self.result
    }
}

impl Blocker {
    pub fn new(kind: BlockerKind, summary: impl Into<String>) -> Self {
        Self {
            kind,
            summary: summary.into(),
        }
    }

    pub const fn kind(&self) -> BlockerKind {
        self.kind
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }
}

impl ContextRevision {
    pub fn new(source: impl Into<String>, revision: Option<ArtifactId>) -> Self {
        Self {
            source: source.into(),
            revision,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub const fn revision(&self) -> Option<ArtifactId> {
        self.revision
    }
}

impl ToolRepairStats {
    pub const fn new(repairs: u32, recovered: u32, rules: Vec<String>) -> Self {
        Self {
            repairs,
            recovered,
            rules,
        }
    }

    pub const fn repairs(&self) -> u32 {
        self.repairs
    }

    pub const fn recovered(&self) -> u32 {
        self.recovered
    }

    pub fn rules(&self) -> &[String] {
        &self.rules
    }

    pub const fn repaired_count(&self) -> usize {
        self.rules.len()
    }
}

/// Typed child result. Evidence, view, and artifacts are references only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentResult {
    agent_id: AgentId,
    status: AgentTerminalStatus,
    summary: String,
    evidence: Vec<EvidenceId>,
    workspace_view: Option<WorkspaceViewId>,
    patch_summary: Option<PatchSummary>,
    artifacts: Vec<ArtifactRef>,
    claims: Vec<Claim>,
    open_questions: Vec<String>,
    blockers: Vec<Blocker>,
    context_lineage: Vec<ContextRevision>,
    tool_repair_stats: ToolRepairStats,
}

/// Spawned agent: immutable spec plus mutable lifecycle/result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Agent {
    spec: AgentSpec,
    state: AgentState,
    stats: AgentStats,
    result: Option<AgentResult>,
}

/// Cooperative cancellation for model operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Typed lifecycle/model failure. Display never echoes task or summary text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentModelError {
    Cancelled,
    InvalidTransition { from: AgentState, to: AgentState },
    IdentityImmutable { field: IdentityField },
    ResultAgentMismatch,
    ResultViewMismatch,
    InvalidTask,
    InvalidPermissionsProfile,
    InvalidModelPolicy,
    TooManyEvidence { limit: usize },
    TooManyArtifacts { limit: usize },
    TooManyClaims { limit: usize },
    TooManyOpenQuestions { limit: usize },
    TooManyBlockers { limit: usize },
    TooManyLineage { limit: usize },
    InvalidSummary,
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

/// Reject illegal lifecycle edges. Terminal states cannot become `running`.
pub fn validate_transition(from: AgentState, to: AgentState) -> Result<(), AgentModelError> {
    if from == to {
        return Err(AgentModelError::InvalidTransition { from, to });
    }
    if from.is_terminal() {
        return Err(AgentModelError::InvalidTransition { from, to });
    }
    let allowed = match from {
        AgentState::Queued => {
            matches!(
                to,
                AgentState::Starting
                    | AgentState::Paused
                    | AgentState::Blocked
                    | AgentState::Cancelled
            )
        }
        AgentState::Starting => {
            matches!(
                to,
                AgentState::Running
                    | AgentState::Blocked
                    | AgentState::Failed
                    | AgentState::Cancelled
            )
        }
        AgentState::Running => matches!(
            to,
            AgentState::WaitingTool
                | AgentState::WaitingApproval
                | AgentState::Paused
                | AgentState::Blocked
                | AgentState::Succeeded
                | AgentState::Failed
                | AgentState::Cancelled
        ),
        AgentState::WaitingTool | AgentState::WaitingApproval => matches!(
            to,
            AgentState::Running
                | AgentState::Paused
                | AgentState::Blocked
                | AgentState::Failed
                | AgentState::Cancelled
        ),
        AgentState::Paused => matches!(
            to,
            AgentState::Queued | AgentState::Running | AgentState::Blocked | AgentState::Cancelled
        ),
        AgentState::Blocked => matches!(
            to,
            AgentState::Queued
                | AgentState::Running
                | AgentState::Paused
                | AgentState::Failed
                | AgentState::Cancelled
        ),
        AgentState::Succeeded | AgentState::Failed | AgentState::Cancelled => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(AgentModelError::InvalidTransition { from, to })
    }
}

/// Reject changes to id, parent, view, or task after spawn.
pub fn validate_identity(current: &AgentSpec, proposed: &AgentSpec) -> Result<(), AgentModelError> {
    if current.id != proposed.id {
        return Err(AgentModelError::IdentityImmutable {
            field: IdentityField::Id,
        });
    }
    if current.parent_id != proposed.parent_id {
        return Err(AgentModelError::IdentityImmutable {
            field: IdentityField::Parent,
        });
    }
    if current.workspace_view_id != proposed.workspace_view_id {
        return Err(AgentModelError::IdentityImmutable {
            field: IdentityField::WorkspaceView,
        });
    }
    if current.task != proposed.task {
        return Err(AgentModelError::IdentityImmutable {
            field: IdentityField::Task,
        });
    }
    Ok(())
}

impl AgentRole {
    pub const ALL: &'static [Self] = &[
        Self::Main,
        Self::Planner,
        Self::Coder,
        Self::Explorer,
        Self::Reviewer,
        Self::Verifier,
        Self::SecurityReviewer,
        Self::ContextCurator,
        Self::Debugger,
        Self::Tester,
        Self::PerformanceReviewer,
        Self::BrowserOperator,
        Self::ReleaseManager,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Planner => "planner",
            Self::Coder => "coder",
            Self::Explorer => "explorer",
            Self::Reviewer => "reviewer",
            Self::Verifier => "verifier",
            Self::SecurityReviewer => "security_reviewer",
            Self::ContextCurator => "context_curator",
            Self::Debugger => "debugger",
            Self::Tester => "tester",
            Self::PerformanceReviewer => "performance_reviewer",
            Self::BrowserOperator => "browser_operator",
            Self::ReleaseManager => "release_manager",
        }
    }
}

impl AgentState {
    pub const ALL: &'static [Self] = &[
        Self::Queued,
        Self::Starting,
        Self::Running,
        Self::WaitingTool,
        Self::WaitingApproval,
        Self::Paused,
        Self::Blocked,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingTool => "waiting_tool",
            Self::WaitingApproval => "waiting_approval",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub const fn terminal_status(self) -> Option<AgentTerminalStatus> {
        match self {
            Self::Succeeded => Some(AgentTerminalStatus::Succeeded),
            Self::Failed => Some(AgentTerminalStatus::Failed),
            Self::Cancelled => Some(AgentTerminalStatus::Cancelled),
            _ => None,
        }
    }
}

impl AgentTerminalStatus {
    pub const ALL: &'static [Self] = &[Self::Succeeded, Self::Failed, Self::Cancelled];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn as_state(self) -> AgentState {
        match self {
            Self::Succeeded => AgentState::Succeeded,
            Self::Failed => AgentState::Failed,
            Self::Cancelled => AgentState::Cancelled,
        }
    }
}

impl IdentityField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Parent => "parent_id",
            Self::WorkspaceView => "workspace_view_id",
            Self::Task => "task",
        }
    }
}

impl ModelPolicyRef {
    pub fn new(name: impl AsRef<str>) -> Result<Self, AgentModelError> {
        name.as_ref()
            .parse::<ModelPolicyName>()
            .map(Self)
            .map_err(|_| AgentModelError::InvalidModelPolicy)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AgentBudget {
    pub const fn new(
        max_tokens: Option<u64>,
        max_cost: Option<u64>,
        max_active_ms: Option<u64>,
        max_tool_calls: Option<u64>,
    ) -> Self {
        Self {
            max_tokens,
            max_cost,
            max_active_ms,
            max_tool_calls,
        }
    }

    pub const fn unlimited() -> Self {
        Self {
            max_tokens: None,
            max_cost: None,
            max_active_ms: None,
            max_tool_calls: None,
        }
    }

    pub const fn max_tokens(self) -> Option<u64> {
        self.max_tokens
    }

    pub const fn max_cost(self) -> Option<u64> {
        self.max_cost
    }

    pub const fn max_active_ms(self) -> Option<u64> {
        self.max_active_ms
    }

    pub const fn max_tool_calls(self) -> Option<u64> {
        self.max_tool_calls
    }
}

impl AgentStats {
    pub const fn new(tokens: u64, cost: u64, active_ms: u64, tool_calls: u64) -> Self {
        Self {
            tokens,
            cost,
            active_ms,
            tool_calls,
        }
    }

    pub const fn tokens(self) -> u64 {
        self.tokens
    }

    pub const fn cost(self) -> u64 {
        self.cost
    }

    pub const fn active_ms(self) -> u64 {
        self.active_ms
    }

    pub const fn tool_calls(self) -> u64 {
        self.tool_calls
    }
}

impl PatchSummary {
    pub const fn new(files_changed: u32, additions: u32, deletions: u32) -> Self {
        Self {
            files_changed,
            additions,
            deletions,
        }
    }

    pub const fn files_changed(self) -> u32 {
        self.files_changed
    }

    pub const fn additions(self) -> u32 {
        self.additions
    }

    pub const fn deletions(self) -> u32 {
        self.deletions
    }
}

/// Construction handle for the eight-field domain [`AgentSpec`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSpecBuilder {
    id: AgentId,
    parent_id: Option<AgentId>,
    role: AgentRole,
    task: String,
    workspace_view_id: WorkspaceViewId,
    model_policy: ModelPolicyRef,
    budget: AgentBudget,
    permissions_profile: String,
}

impl AgentSpecBuilder {
    pub fn parent_id(mut self, parent_id: Option<AgentId>) -> Self {
        self.parent_id = parent_id;
        self
    }

    pub fn model_policy(mut self, model_policy: ModelPolicyRef) -> Self {
        self.model_policy = model_policy;
        self
    }

    pub fn budget(mut self, budget: AgentBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn permissions_profile(mut self, permissions_profile: impl Into<String>) -> Self {
        self.permissions_profile = permissions_profile.into();
        self
    }

    pub fn build(self) -> Result<AgentSpec, AgentModelError> {
        let spec = AgentSpec {
            id: self.id,
            parent_id: self.parent_id,
            role: self.role,
            task: self.task,
            workspace_view_id: self.workspace_view_id,
            model_policy: self.model_policy,
            budget: self.budget,
            permissions_profile: self.permissions_profile,
        };
        spec.validate()?;
        Ok(spec)
    }
}

impl AgentSpec {
    pub fn builder(
        id: AgentId,
        role: AgentRole,
        task: impl Into<String>,
        workspace_view_id: WorkspaceViewId,
    ) -> AgentSpecBuilder {
        AgentSpecBuilder {
            id,
            parent_id: None,
            role,
            task: task.into(),
            workspace_view_id,
            model_policy: ModelPolicyRef::default(),
            budget: AgentBudget::unlimited(),
            permissions_profile: String::new(),
        }
    }

    fn validate(&self) -> Result<(), AgentModelError> {
        if self.task.is_empty() || self.task.len() > MAX_TASK_BYTES {
            return Err(AgentModelError::InvalidTask);
        }
        if self.permissions_profile.is_empty()
            || self.permissions_profile.len() > MAX_PERMISSIONS_PROFILE_BYTES
        {
            return Err(AgentModelError::InvalidPermissionsProfile);
        }
        Ok(())
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn parent_id(&self) -> Option<AgentId> {
        self.parent_id
    }

    pub fn role(&self) -> AgentRole {
        self.role
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn workspace_view_id(&self) -> WorkspaceViewId {
        self.workspace_view_id
    }

    pub fn model_policy(&self) -> &ModelPolicyRef {
        &self.model_policy
    }

    pub fn budget(&self) -> AgentBudget {
        self.budget
    }

    pub fn permissions_profile(&self) -> &str {
        &self.permissions_profile
    }
}

impl AgentResult {
    pub fn new(
        agent_id: AgentId,
        status: AgentTerminalStatus,
        summary: impl Into<String>,
        evidence: Vec<EvidenceId>,
        workspace_view: Option<WorkspaceViewId>,
        patch_summary: Option<PatchSummary>,
        artifacts: Vec<ArtifactRef>,
    ) -> Result<Self, AgentModelError> {
        let result = Self {
            agent_id,
            status,
            summary: summary.into(),
            evidence,
            workspace_view,
            patch_summary,
            artifacts,
            claims: Vec::new(),
            open_questions: Vec::new(),
            blockers: Vec::new(),
            context_lineage: Vec::new(),
            tool_repair_stats: ToolRepairStats::default(),
        };
        result.validate()?;
        Ok(result)
    }

    /// Attach structured observable claims.
    pub fn with_claims(
        mut self,
        claims: impl IntoIterator<Item = Claim>,
    ) -> Result<Self, AgentModelError> {
        self.claims = claims.into_iter().collect();
        self.validate()?;
        Ok(self)
    }

    /// Attach bounded open questions.
    pub fn with_open_questions(
        mut self,
        questions: impl IntoIterator<Item = String>,
    ) -> Result<Self, AgentModelError> {
        self.open_questions = questions.into_iter().collect();
        self.validate()?;
        Ok(self)
    }

    /// Attach structured blockers.
    pub fn with_blockers(
        mut self,
        blockers: impl IntoIterator<Item = Blocker>,
    ) -> Result<Self, AgentModelError> {
        self.blockers = blockers.into_iter().collect();
        self.validate()?;
        Ok(self)
    }

    /// Attach context provenance revisions.
    pub fn with_context_lineage(
        mut self,
        lineage: impl IntoIterator<Item = ContextRevision>,
    ) -> Result<Self, AgentModelError> {
        self.context_lineage = lineage.into_iter().collect();
        self.validate()?;
        Ok(self)
    }

    /// Attach bounded tool-repair statistics.
    pub fn with_tool_repair_stats(
        mut self,
        stats: ToolRepairStats,
    ) -> Result<Self, AgentModelError> {
        self.tool_repair_stats = stats;
        self.validate()?;
        Ok(self)
    }

    pub fn claims(&self) -> &[Claim] {
        &self.claims
    }

    pub fn open_questions(&self) -> &[String] {
        &self.open_questions
    }

    pub fn blockers(&self) -> &[Blocker] {
        &self.blockers
    }

    pub fn context_lineage(&self) -> &[ContextRevision] {
        &self.context_lineage
    }

    pub fn tool_repair_stats(&self) -> &ToolRepairStats {
        &self.tool_repair_stats
    }

    fn validate(&self) -> Result<(), AgentModelError> {
        if self.summary.is_empty() || self.summary.len() > MAX_SUMMARY_BYTES {
            return Err(AgentModelError::InvalidSummary);
        }
        if self.evidence.len() > MAX_EVIDENCE {
            return Err(AgentModelError::TooManyEvidence {
                limit: MAX_EVIDENCE,
            });
        }
        if self.artifacts.len() > MAX_ARTIFACTS {
            return Err(AgentModelError::TooManyArtifacts {
                limit: MAX_ARTIFACTS,
            });
        }
        if self.claims.len() > MAX_CLAIMS {
            return Err(AgentModelError::TooManyClaims { limit: MAX_CLAIMS });
        }
        if self.open_questions.len() > MAX_OPEN_QUESTIONS {
            return Err(AgentModelError::TooManyOpenQuestions {
                limit: MAX_OPEN_QUESTIONS,
            });
        }
        if self.blockers.len() > MAX_BLOCKERS {
            return Err(AgentModelError::TooManyBlockers {
                limit: MAX_BLOCKERS,
            });
        }
        if self.context_lineage.len() > MAX_LINEAGE {
            return Err(AgentModelError::TooManyLineage { limit: MAX_LINEAGE });
        }
        if self.tool_repair_stats.repaired_count() > MAX_REPAIR_RULES
            || self.tool_repair_stats.rules().len() > MAX_REPAIR_RULES
        {
            return Err(AgentModelError::TooManyClaims {
                limit: MAX_REPAIR_RULES,
            });
        }
        for claim in &self.claims {
            if claim.text().len() > MAX_CLAIM_TEXT_BYTES {
                return Err(AgentModelError::InvalidSummary);
            }
        }
        Ok(())
    }

    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub fn status(&self) -> AgentTerminalStatus {
        self.status
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn evidence(&self) -> &[EvidenceId] {
        &self.evidence
    }

    pub fn workspace_view(&self) -> Option<WorkspaceViewId> {
        self.workspace_view
    }

    pub fn patch_summary(&self) -> Option<PatchSummary> {
        self.patch_summary
    }

    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }
}

impl Agent {
    /// Admit a spec in `queued`. Identity is frozen at this point.
    pub fn spawn(spec: AgentSpec, cancel: &CancellationToken) -> Result<Self, AgentModelError> {
        cancel.check()?;
        spec.validate()?;
        Ok(Self {
            spec,
            state: AgentState::Queued,
            stats: AgentStats::default(),
            result: None,
        })
    }

    pub fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn stats(&self) -> AgentStats {
        self.stats
    }

    pub fn result(&self) -> Option<&AgentResult> {
        self.result.as_ref()
    }

    pub fn record_stats(
        &mut self,
        stats: AgentStats,
        cancel: &CancellationToken,
    ) -> Result<(), AgentModelError> {
        cancel.check()?;
        self.stats = stats;
        Ok(())
    }

    pub fn transition(
        &mut self,
        next: AgentState,
        cancel: &CancellationToken,
    ) -> Result<AgentState, AgentModelError> {
        cancel.check()?;
        validate_transition(self.state, next)?;
        self.state = next;
        if !next.is_terminal() {
            self.result = None;
        }
        Ok(self.state)
    }

    /// Persist a typed result and enter its terminal status.
    pub fn complete(
        &mut self,
        result: AgentResult,
        cancel: &CancellationToken,
    ) -> Result<AgentState, AgentModelError> {
        cancel.check()?;
        result.validate()?;
        if result.agent_id != self.spec.id {
            return Err(AgentModelError::ResultAgentMismatch);
        }
        if let Some(view) = result.workspace_view
            && view != self.spec.workspace_view_id
        {
            return Err(AgentModelError::ResultViewMismatch);
        }
        let next = result.status.as_state();
        validate_transition(self.state, next)?;
        self.state = next;
        self.result = Some(result);
        Ok(self.state)
    }

    /// Reject a proposed spec that would rewrite identity or scope.
    pub fn replace_spec(&self, proposed: AgentSpec) -> Result<(), AgentModelError> {
        validate_identity(&self.spec, &proposed)
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

    pub fn check(&self) -> Result<(), AgentModelError> {
        if self.is_cancelled() {
            Err(AgentModelError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for AgentTerminalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for IdentityField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ModelPolicyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentRole {
    type Err = AgentModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for AgentState {
    type Err = AgentModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for AgentTerminalStatus {
    type Err = AgentModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_closed(s, Self::ALL, Self::as_str)
    }
}

impl FromStr for ModelPolicyRef {
    type Err = AgentModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for AgentModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("agent model operation cancelled"),
            Self::InvalidTransition { from, to } => {
                write!(f, "illegal agent state transition {from} -> {to}")
            }
            Self::IdentityImmutable { field } => {
                write!(f, "agent {field} is immutable after spawn")
            }
            Self::ResultAgentMismatch => f.write_str("agent result id does not match spec"),
            Self::ResultViewMismatch => f.write_str("agent result view does not match spec"),
            Self::InvalidTask => f.write_str("agent task is empty or exceeds the size bound"),
            Self::InvalidPermissionsProfile => {
                f.write_str("permissions profile is empty or exceeds the size bound")
            }
            Self::InvalidModelPolicy => f.write_str("model policy name must be non-empty"),
            Self::TooManyEvidence { limit } => {
                write!(f, "agent result exceeds {limit} evidence refs")
            }
            Self::TooManyArtifacts { limit } => {
                write!(f, "agent result exceeds {limit} artifact refs")
            }
            Self::TooManyClaims { limit } => {
                write!(f, "agent result exceeds {limit} claims")
            }
            Self::TooManyOpenQuestions { limit } => {
                write!(f, "agent result exceeds {limit} open questions")
            }
            Self::TooManyBlockers { limit } => {
                write!(f, "agent result exceeds {limit} blockers")
            }
            Self::TooManyLineage { limit } => {
                write!(f, "agent result exceeds {limit} context revisions")
            }
            Self::InvalidSummary => {
                f.write_str("agent result summary is empty or exceeds the size bound")
            }
            Self::UnknownVariant => f.write_str("unknown agent model variant"),
            Self::UnsupportedSchema => f.write_str("unsupported agent schema"),
            Self::UnsupportedSchemaVersion => f.write_str("unsupported agent schema version"),
        }
    }
}

impl Error for AgentModelError {}

impl From<ModelPolicyNameParseError> for AgentModelError {
    fn from(_: ModelPolicyNameParseError) -> Self {
        Self::InvalidModelPolicy
    }
}

fn parse_closed<T: Copy>(
    raw: &str,
    all: &[T],
    as_str: fn(T) -> &'static str,
) -> Result<T, AgentModelError> {
    for item in all {
        if as_str(*item) == raw {
            return Ok(*item);
        }
    }
    Err(AgentModelError::UnknownVariant)
}

fn deserialize_closed<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: FromStr<Err = AgentModelError>,
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse()
        .map_err(|_| de::Error::unknown_variant(&raw, &[]))
}

impl Serialize for AgentRole {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for AgentState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for AgentTerminalStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for ModelPolicyRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentRole {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for AgentState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for AgentTerminalStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_closed(deserializer)
    }
}

impl<'de> Deserialize<'de> for ModelPolicyRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        ModelPolicyRef::new(raw).map_err(de::Error::custom)
    }
}

impl Serialize for AgentBudget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AgentBudget", BUDGET_FIELDS.len())?;
        state.serialize_field("max_tokens", &self.max_tokens)?;
        state.serialize_field("max_cost", &self.max_cost)?;
        state.serialize_field("max_active_ms", &self.max_active_ms)?;
        state.serialize_field("max_tool_calls", &self.max_tool_calls)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AgentBudget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            max_tokens: Option<u64>,
            max_cost: Option<u64>,
            max_active_ms: Option<u64>,
            max_tool_calls: Option<u64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            max_tokens: raw.max_tokens,
            max_cost: raw.max_cost,
            max_active_ms: raw.max_active_ms,
            max_tool_calls: raw.max_tool_calls,
        })
    }
}

impl Serialize for AgentStats {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AgentStats", STATS_FIELDS.len())?;
        state.serialize_field("tokens", &self.tokens)?;
        state.serialize_field("cost", &self.cost)?;
        state.serialize_field("active_ms", &self.active_ms)?;
        state.serialize_field("tool_calls", &self.tool_calls)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AgentStats {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            tokens: u64,
            cost: u64,
            active_ms: u64,
            tool_calls: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            tokens: raw.tokens,
            cost: raw.cost,
            active_ms: raw.active_ms,
            tool_calls: raw.tool_calls,
        })
    }
}

impl Serialize for PatchSummary {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PatchSummary", PATCH_SUMMARY_FIELDS.len())?;
        state.serialize_field("files_changed", &self.files_changed)?;
        state.serialize_field("additions", &self.additions)?;
        state.serialize_field("deletions", &self.deletions)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for PatchSummary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            files_changed: u32,
            additions: u32,
            deletions: u32,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            files_changed: raw.files_changed,
            additions: raw.additions,
            deletions: raw.deletions,
        })
    }
}

impl Serialize for AgentSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AgentSpec", SPEC_FIELDS.len())?;
        state.serialize_field("schema", AGENT_SPEC_SCHEMA)?;
        state.serialize_field("schema_version", &AGENT_SCHEMA_VERSION)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("parent_id", &self.parent_id)?;
        state.serialize_field("role", &self.role)?;
        state.serialize_field("task", &self.task)?;
        state.serialize_field("workspace_view_id", &self.workspace_view_id)?;
        state.serialize_field("model_policy", &self.model_policy)?;
        state.serialize_field("budget", &self.budget)?;
        state.serialize_field("permissions_profile", &self.permissions_profile)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AgentSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            id: AgentId,
            parent_id: Option<AgentId>,
            role: AgentRole,
            task: String,
            workspace_view_id: WorkspaceViewId,
            model_policy: ModelPolicyRef,
            budget: AgentBudget,
            permissions_profile: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != AGENT_SPEC_SCHEMA {
            return Err(de::Error::custom(AgentModelError::UnsupportedSchema));
        }
        if raw.schema_version != AGENT_SCHEMA_VERSION {
            return Err(de::Error::custom(AgentModelError::UnsupportedSchemaVersion));
        }
        AgentSpec::builder(raw.id, raw.role, raw.task, raw.workspace_view_id)
            .parent_id(raw.parent_id)
            .model_policy(raw.model_policy)
            .budget(raw.budget)
            .permissions_profile(raw.permissions_profile)
            .build()
            .map_err(de::Error::custom)
    }
}

impl Serialize for AgentResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AgentResult", RESULT_FIELDS.len())?;
        state.serialize_field("schema", AGENT_RESULT_SCHEMA)?;
        state.serialize_field("schema_version", &AGENT_SCHEMA_VERSION)?;
        state.serialize_field("agent_id", &self.agent_id)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("summary", &self.summary)?;
        state.serialize_field("evidence", &self.evidence)?;
        state.serialize_field("workspace_view", &self.workspace_view)?;
        state.serialize_field("patch_summary", &self.patch_summary)?;
        state.serialize_field("artifacts", &self.artifacts)?;
        state.serialize_field("claims", &self.claims)?;
        state.serialize_field("open_questions", &self.open_questions)?;
        state.serialize_field("blockers", &self.blockers)?;
        state.serialize_field("context_lineage", &self.context_lineage)?;
        state.serialize_field("tool_repair_stats", &self.tool_repair_stats)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for AgentResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            agent_id: AgentId,
            status: AgentTerminalStatus,
            summary: String,
            evidence: Vec<EvidenceId>,
            workspace_view: Option<WorkspaceViewId>,
            patch_summary: Option<PatchSummary>,
            artifacts: Vec<ArtifactRef>,
            #[serde(default)]
            claims: Vec<Claim>,
            #[serde(default)]
            open_questions: Vec<String>,
            #[serde(default)]
            blockers: Vec<Blocker>,
            #[serde(default)]
            context_lineage: Vec<ContextRevision>,
            #[serde(default)]
            tool_repair_stats: ToolRepairStats,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != AGENT_RESULT_SCHEMA {
            return Err(de::Error::custom(AgentModelError::UnsupportedSchema));
        }
        if raw.schema_version != AGENT_SCHEMA_VERSION {
            return Err(de::Error::custom(AgentModelError::UnsupportedSchemaVersion));
        }
        let result = AgentResult::new(
            raw.agent_id,
            raw.status,
            raw.summary,
            raw.evidence,
            raw.workspace_view,
            raw.patch_summary,
            raw.artifacts,
        )
        .map_err(de::Error::custom)?;
        result
            .with_claims(raw.claims)
            .and_then(|r| r.with_open_questions(raw.open_questions))
            .and_then(|r| r.with_blockers(raw.blockers))
            .and_then(|r| r.with_context_lineage(raw.context_lineage))
            .and_then(|r| r.with_tool_repair_stats(raw.tool_repair_stats))
            .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ArtifactId, RedactionClass};
    use std::str::FromStr;

    const AGENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const PARENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
    const VIEW_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const EVIDENCE_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
    const OTHER_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789af";
    const ABC_ARTIFACT: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    const GOLDEN_SPEC: &str = r#"{"schema":"rapidlm.agent_spec","schema_version":1,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","parent_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","role":"coder","task":"review auth crate","workspace_view_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","model_policy":"balanced","budget":{"max_tokens":100000,"max_cost":2500000,"max_active_ms":600000,"max_tool_calls":40},"permissions_profile":"default"}"#;

    const GOLDEN_RESULT: &str = r#"{"schema":"rapidlm.agent_result","schema_version":1,"agent_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","status":"succeeded","summary":"reviewed auth crate","evidence":["018f3c8a-7e2b-7a10-8c4d-0123456789ae"],"workspace_view":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","patch_summary":{"files_changed":1,"additions":12,"deletions":3},"artifacts":[{"id":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","media_type":"text/plain","bytes":3,"redaction":"public"}],"claims":[],"open_questions":[],"blockers":[],"context_lineage":[],"tool_repair_stats":{"repairs":0,"recovered":0,"rules":[]}}"#;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn parse_id<T: FromStr>(raw: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        raw.parse().expect("id")
    }

    fn sample_spec() -> AgentSpec {
        AgentSpec::builder(
            parse_id(AGENT_ID),
            AgentRole::Coder,
            "review auth crate",
            parse_id(VIEW_ID),
        )
        .parent_id(Some(parse_id(PARENT_ID)))
        .model_policy(ModelPolicyRef::new("balanced").expect("policy"))
        .budget(AgentBudget::new(
            Some(100_000),
            Some(2_500_000),
            Some(600_000),
            Some(40),
        ))
        .permissions_profile("default")
        .build()
        .expect("spec")
    }

    fn sample_result() -> AgentResult {
        AgentResult::new(
            parse_id(AGENT_ID),
            AgentTerminalStatus::Succeeded,
            "reviewed auth crate",
            vec![parse_id(EVIDENCE_ID)],
            Some(parse_id(VIEW_ID)),
            Some(PatchSummary::new(1, 12, 3)),
            vec![ArtifactRef::new(
                ArtifactId::from_bytes(b"abc"),
                "text/plain",
                3,
                RedactionClass::Public,
            )],
        )
        .expect("result")
    }

    fn running_agent() -> Agent {
        let mut agent = Agent::spawn(sample_spec(), &cancel()).expect("spawn");
        agent
            .transition(AgentState::Starting, &cancel())
            .expect("starting");
        agent
            .transition(AgentState::Running, &cancel())
            .expect("running");
        agent
    }

    #[test]
    fn spec_golden_round_trips() {
        let spec = sample_spec();
        let json = serde_json::to_string(&spec).expect("serialize spec");
        assert_eq!(json, GOLDEN_SPEC);
        let decoded: AgentSpec = serde_json::from_str(GOLDEN_SPEC).expect("decode spec");
        assert_eq!(decoded, spec);
        assert_eq!(decoded.id(), parse_id(AGENT_ID));
        assert_eq!(decoded.parent_id(), Some(parse_id(PARENT_ID)));
        assert_eq!(
            decoded.workspace_view_id(),
            parse_id::<WorkspaceViewId>(VIEW_ID)
        );
        assert_eq!(decoded.role(), AgentRole::Coder);
        assert_eq!(decoded.budget().max_tokens(), Some(100_000));
        assert_eq!(decoded.budget().max_cost(), Some(2_500_000));
    }

    #[test]
    fn result_golden_round_trips_evidence_view_and_artifacts() {
        let result = sample_result();
        let json = serde_json::to_string(&result).expect("serialize result");
        assert_eq!(json, GOLDEN_RESULT);
        assert!(json.contains(ABC_ARTIFACT));
        assert!(json.contains("workspace_view"));
        assert!(json.contains("evidence"));
        assert!(json.contains("artifacts"));
        let decoded: AgentResult = serde_json::from_str(GOLDEN_RESULT).expect("decode result");
        assert_eq!(decoded, result);
        assert_eq!(decoded.evidence().len(), 1);
        assert_eq!(decoded.workspace_view(), Some(parse_id(VIEW_ID)));
        assert_eq!(decoded.artifacts().len(), 1);
        assert_eq!(decoded.status(), AgentTerminalStatus::Succeeded);
    }

    #[test]
    fn result_structured_fields_round_trip_and_never_leak_payloads() {
        let claim = Claim {
            criterion_id: Some("c1".to_owned()),
            text: "unit tests pass".to_owned(),
            result: ClaimResult::Satisfied,
        };
        let blocker = Blocker {
            kind: BlockerKind::Policy,
            summary: "need approval".to_owned(),
        };
        let revision = ContextRevision {
            source: "repo/AGENTS.md".to_owned(),
            revision: Some(ArtifactId::from_bytes(b"lineage")),
        };
        let stats = ToolRepairStats::new(2, 1, vec!["numeric_scalar_coercion".to_owned()]);
        let result = sample_result()
            .with_claims([claim.clone()])
            .expect("claims")
            .with_open_questions(["why omitted?".to_owned()])
            .expect("questions")
            .with_blockers([blocker.clone()])
            .expect("blockers")
            .with_context_lineage([revision])
            .expect("lineage")
            .with_tool_repair_stats(stats.clone())
            .expect("stats");
        let json = serde_json::to_string(&result).expect("json");
        let decoded: AgentResult = serde_json::from_str(&json).expect("decoded");
        assert_eq!(decoded.claims(), result.claims());
        assert_eq!(decoded.open_questions(), result.open_questions());
        assert_eq!(decoded.blockers(), result.blockers());
        assert_eq!(decoded.context_lineage(), result.context_lineage());
        assert_eq!(decoded.tool_repair_stats(), &stats);
        assert_eq!(decoded.claims()[0].result(), ClaimResult::Satisfied);
        assert_eq!(decoded.blockers()[0].kind(), BlockerKind::Policy);
        assert_eq!(decoded.context_lineage()[0].source(), "repo/AGENTS.md");
        // No raw secret/payload leaks into the serialized result.
        assert!(!json.contains("hunter2"));
        assert!(!json.contains("raw-tool-output"));
    }

    #[test]
    fn result_structured_fields_are_bounded_and_fail_closed() {
        let too_many = (0..=MAX_CLAIMS)
            .map(|i| Claim {
                criterion_id: None,
                text: format!("claim {i}"),
                result: ClaimResult::Unverified,
            })
            .collect::<Vec<_>>();
        assert!(sample_result().with_claims(too_many).is_err());
        let long = Claim {
            criterion_id: None,
            text: "x".repeat(MAX_CLAIM_TEXT_BYTES + 1),
            result: ClaimResult::Unverified,
        };
        assert!(sample_result().with_claims([long]).is_err());
        let over_rules = ToolRepairStats::new(
            100,
            0,
            (0..=MAX_REPAIR_RULES).map(|i| format!("rule{i}")).collect(),
        );
        assert!(sample_result().with_tool_repair_stats(over_rules).is_err());
    }

    #[test]
    fn state_wire_forms_match_domain_and_kernel() {
        assert_eq!(AgentState::Succeeded.as_str(), "succeeded");
        assert_eq!(AgentState::WaitingTool.as_str(), "waiting_tool");
        assert_eq!(AgentState::WaitingApproval.as_str(), "waiting_approval");
        assert_eq!(
            "succeeded".parse::<AgentState>().expect("state"),
            AgentState::Succeeded
        );
        assert_eq!(
            "cancelled".parse::<AgentTerminalStatus>().expect("status"),
            AgentTerminalStatus::Cancelled
        );
        assert!("completed".parse::<AgentState>().is_err());
        assert!("running".parse::<AgentTerminalStatus>().is_err());
    }

    #[test]
    fn transition_validator_rejects_terminal_to_running() {
        for terminal in [
            AgentState::Succeeded,
            AgentState::Failed,
            AgentState::Cancelled,
        ] {
            let err = validate_transition(terminal, AgentState::Running).expect_err("rejected");
            assert_eq!(
                err,
                AgentModelError::InvalidTransition {
                    from: terminal,
                    to: AgentState::Running
                }
            );
        }
    }

    #[test]
    fn transition_validator_rejects_illegal_edges() {
        assert!(validate_transition(AgentState::Queued, AgentState::Running).is_err());
        assert!(validate_transition(AgentState::Queued, AgentState::Succeeded).is_err());
        assert!(validate_transition(AgentState::WaitingTool, AgentState::Starting).is_err());
        assert!(validate_transition(AgentState::Running, AgentState::Queued).is_err());
        assert!(validate_transition(AgentState::Succeeded, AgentState::Failed).is_err());
    }

    #[test]
    fn legal_lifecycle_reaches_succeeded() {
        let mut agent = running_agent();
        agent
            .transition(AgentState::WaitingTool, &cancel())
            .expect("tool");
        agent
            .transition(AgentState::Running, &cancel())
            .expect("resume");
        let state = agent
            .complete(sample_result(), &cancel())
            .expect("complete");
        assert_eq!(state, AgentState::Succeeded);
        assert!(agent.state().is_terminal());
        assert!(agent.result().is_some());
    }

    #[test]
    fn identity_and_parent_and_view_are_immutable_after_spawn() {
        let agent = Agent::spawn(sample_spec(), &cancel()).expect("spawn");
        let mut rewritten = sample_spec();
        rewritten.id = parse_id(OTHER_ID);
        assert_eq!(
            agent.replace_spec(rewritten),
            Err(AgentModelError::IdentityImmutable {
                field: IdentityField::Id
            })
        );

        let mut rewritten = sample_spec();
        rewritten.parent_id = None;
        assert_eq!(
            validate_identity(agent.spec(), &rewritten),
            Err(AgentModelError::IdentityImmutable {
                field: IdentityField::Parent
            })
        );

        let mut rewritten = sample_spec();
        rewritten.workspace_view_id = parse_id(OTHER_ID);
        assert_eq!(
            validate_identity(agent.spec(), &rewritten),
            Err(AgentModelError::IdentityImmutable {
                field: IdentityField::WorkspaceView
            })
        );

        let mut rewritten = sample_spec();
        rewritten.task = "different scope".to_owned();
        assert_eq!(
            validate_identity(agent.spec(), &rewritten),
            Err(AgentModelError::IdentityImmutable {
                field: IdentityField::Task
            })
        );
    }

    #[test]
    fn complete_rejects_foreign_identity() {
        let mut agent = running_agent();
        let foreign = AgentResult::new(
            parse_id(OTHER_ID),
            AgentTerminalStatus::Succeeded,
            "reviewed auth crate",
            vec![parse_id(EVIDENCE_ID)],
            Some(parse_id(VIEW_ID)),
            None,
            Vec::new(),
        )
        .expect("foreign");
        assert_eq!(
            agent.complete(foreign, &cancel()),
            Err(AgentModelError::ResultAgentMismatch)
        );

        let wrong_view = AgentResult::new(
            parse_id(AGENT_ID),
            AgentTerminalStatus::Succeeded,
            "reviewed auth crate",
            Vec::new(),
            Some(parse_id(OTHER_ID)),
            None,
            Vec::new(),
        )
        .expect("view");
        assert_eq!(
            agent.complete(wrong_view, &cancel()),
            Err(AgentModelError::ResultViewMismatch)
        );
    }

    #[test]
    fn complete_from_queued_is_rejected() {
        let mut agent = Agent::spawn(sample_spec(), &cancel()).expect("spawn");
        let err = agent
            .complete(sample_result(), &cancel())
            .expect_err("queued");
        assert!(
            matches!(err, AgentModelError::InvalidTransition { from, to } if from == AgentState::Queued && to == AgentState::Succeeded)
        );
    }

    #[test]
    fn empty_task_and_unknown_fields_fail_closed() {
        assert!(matches!(
            AgentSpec::builder(parse_id(AGENT_ID), AgentRole::Main, "", parse_id(VIEW_ID))
                .permissions_profile("default")
                .build(),
            Err(AgentModelError::InvalidTask)
        ));
        assert!(ModelPolicyRef::new("").is_err());
        assert!(
            serde_json::from_str::<AgentSpec>(
                &GOLDEN_SPEC.replace(r#""role":"coder""#, r#""role":"coder","extra":true"#)
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<AgentResult>(&GOLDEN_RESULT.replace(
                r#""status":"succeeded""#,
                r#""status":"succeeded","lease":"x""#
            ))
            .is_err()
        );
        assert!(serde_json::from_str::<AgentState>("\"COMPLETE\"").is_err());
        assert!(serde_json::from_str::<AgentRole>("\"* \"").is_err());
    }

    #[test]
    fn cancellation_is_typed_and_checked() {
        let token = cancel();
        token.cancel();
        assert_eq!(
            Agent::spawn(sample_spec(), &token).expect_err("cancelled"),
            AgentModelError::Cancelled
        );
        let mut agent = Agent::spawn(sample_spec(), &cancel()).expect("spawn");
        assert_eq!(
            agent.transition(AgentState::Starting, &token),
            Err(AgentModelError::Cancelled)
        );
    }

    #[test]
    fn evidence_and_artifact_caps_are_enforced() {
        let too_many_evidence = vec![parse_id::<EvidenceId>(EVIDENCE_ID); MAX_EVIDENCE + 1];
        assert!(matches!(
            AgentResult::new(
                parse_id(AGENT_ID),
                AgentTerminalStatus::Failed,
                "blocked on evidence cap",
                too_many_evidence,
                None,
                None,
                Vec::new(),
            ),
            Err(AgentModelError::TooManyEvidence {
                limit: MAX_EVIDENCE
            })
        ));
        let artifact = ArtifactRef::new(
            ArtifactId::from_bytes(b"abc"),
            "text/plain",
            3,
            RedactionClass::Public,
        );
        let too_many_artifacts = vec![artifact; MAX_ARTIFACTS + 1];
        assert!(matches!(
            AgentResult::new(
                parse_id(AGENT_ID),
                AgentTerminalStatus::Failed,
                "blocked on artifact cap",
                Vec::new(),
                None,
                None,
                too_many_artifacts,
            ),
            Err(AgentModelError::TooManyArtifacts {
                limit: MAX_ARTIFACTS
            })
        ));
    }

    #[test]
    fn stats_are_observational_and_do_not_mutate_identity() {
        let mut agent = running_agent();
        let before = agent.spec().clone();
        agent
            .record_stats(AgentStats::new(10, 20, 30, 1), &cancel())
            .expect("stats");
        assert_eq!(agent.stats().tokens(), 10);
        assert_eq!(agent.spec(), &before);
    }
}
