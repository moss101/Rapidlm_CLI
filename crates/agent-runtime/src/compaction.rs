//! Structured session compaction.
//!
//! `compact` writes a durable continuation record from machine-owned session
//! state. Goal, budgets, and unresolved blockers are copied, never inferred.
//! Hidden reasoning, capability leases, and secret plaintext cannot occupy
//! artifact fields.

use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use protocol::{
    AgentId, ArtifactId, ArtifactRef, ErrorCode, EvidenceId, JobId, RedactionClass, SessionId,
};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::model::CancellationToken;
use crate::goal::state::GoalSnapshot;

/// Wire schema name for [`CompactionArtifact`].
pub const COMPACTION_ARTIFACT_SCHEMA: &str = "rapidlm.compaction_artifact";

/// Wire schema name for [`CompactionEvent`].
pub const COMPACTION_EVENT_SCHEMA: &str = "rapidlm.compaction_event";

/// v1 schema version for compaction artifacts and events.
pub const COMPACTION_SCHEMA_VERSION: u16 = 1;

/// Ledger kind recorded with a successful compact.
pub const COMPACTION_EVENT_KIND: &str = "session.compacted";

/// Media type of the canonical artifact bytes.
pub const COMPACTION_MEDIA_TYPE: &str = "application/vnd.rapidlm.compaction+json";

/// Default wall-clock budget for one compact.
pub const DEFAULT_COMPACT_TIMEOUT: Duration = Duration::from_secs(1);

/// Default decision cap.
pub const DEFAULT_MAX_DECISIONS: usize = 64;

/// Default changed-file cap.
pub const DEFAULT_MAX_FILES: usize = 256;

/// Default evidence-id cap.
pub const DEFAULT_MAX_EVIDENCE: usize = 256;

/// Default agent-handle plus job-handle cap.
pub const DEFAULT_MAX_HANDLES: usize = 256;

/// Default read-hash cap.
pub const DEFAULT_MAX_READ_HASHES: usize = 8_192;

/// Default unresolved-blocker cap.
pub const DEFAULT_MAX_BLOCKERS: usize = 64;

/// Default user-constraint plus policy-constraint cap (each list).
pub const DEFAULT_MAX_CONSTRAINTS: usize = 64;

/// Default unresolved-question cap.
pub const DEFAULT_MAX_QUESTIONS: usize = 64;

/// Default UTF-8 byte cap for one text field.
pub const DEFAULT_MAX_TEXT_BYTES: usize = 4 * 1024;

/// Default symbols retained on one changed file.
pub const DEFAULT_MAX_SYMBOLS: usize = 64;

/// Default serialized artifact byte cap.
pub const DEFAULT_MAX_ARTIFACT_BYTES: usize = 256 * 1024;

const CANCEL_STRIDE: usize = 16;

const ARTIFACT_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "session_id",
    "source_range",
    "goal",
    "constraints",
    "unresolved_questions",
    "decisions",
    "files",
    "evidence",
    "handles",
    "policy_constraints",
    "read_hashes",
    "blockers",
    "next_step",
];

const EVENT_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "kind",
    "session_id",
    "source_range",
    "artifact",
];

const RANGE_FIELDS: &[&str] = &["from_seq", "to_seq"];

const DECISION_FIELDS: &[&str] = &["statement", "reason"];

const FILE_FIELDS: &[&str] = &["path", "symbols"];

const HANDLES_FIELDS: &[&str] = &["agents", "jobs"];

const READ_HASH_FIELDS: &[&str] = &["locator", "content_hash", "stale"];

const BLOCKER_FIELDS: &[&str] = &["text", "uncertain"];

/// Keys that must never appear on a compaction artifact or event.
const FORBIDDEN_KEYS: &[&str] = &[
    "action_hash",
    "api_key",
    "capability_lease",
    "capability_leases",
    "chain_of_thought",
    "hidden_reasoning",
    "lease",
    "lease_id",
    "leases",
    "password",
    "plaintext",
    "reasoning_chain",
    "secret",
    "secrets",
];

/// Inclusive ledger sequence window compacted into one artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct EventRange {
    from_seq: u64,
    to_seq: u64,
}

/// Observable decision retained for continuation. Not hidden reasoning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionDecision {
    statement: String,
    reason: String,
}

/// Exact file and symbols changed in the compacted range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFile {
    path: String,
    symbols: Vec<String>,
}

/// Agent and job identities retained as handles. No live process objects.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompactionHandles {
    agents: Vec<AgentId>,
    jobs: Vec<JobId>,
}

/// Read-set locator plus the content hash shown to the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadHash {
    locator: String,
    content_hash: ArtifactId,
    stale: bool,
}

/// Unresolved blocker copied from runtime state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedBlocker {
    text: String,
    uncertain: bool,
}

/// Resource bounds for one [`compact`] call. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct CompactionLimits {
    max_decisions: usize,
    max_files: usize,
    max_evidence: usize,
    max_handles: usize,
    max_read_hashes: usize,
    max_blockers: usize,
    max_constraints: usize,
    max_questions: usize,
    max_symbols: usize,
    max_text_bytes: usize,
    max_artifact_bytes: usize,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Machine-owned session projection to compact. Observer text is data only.
#[derive(Clone, Debug)]
pub struct CompactionInput {
    session_id: SessionId,
    source_range: EventRange,
    goal: Option<GoalSnapshot>,
    constraints: Vec<String>,
    unresolved_questions: Vec<String>,
    decisions: Vec<CompactionDecision>,
    files: Vec<ChangedFile>,
    evidence: Vec<EvidenceId>,
    handles: CompactionHandles,
    policy_constraints: Vec<String>,
    read_hashes: Vec<ReadHash>,
    blockers: Vec<UnresolvedBlocker>,
    next_step: Option<String>,
    excluded_literals: Vec<String>,
    limits: CompactionLimits,
}

/// Durable structured continuation record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionArtifact {
    session_id: SessionId,
    source_range: EventRange,
    goal: Option<GoalSnapshot>,
    constraints: Vec<String>,
    unresolved_questions: Vec<String>,
    decisions: Vec<CompactionDecision>,
    files: Vec<ChangedFile>,
    evidence: Vec<EvidenceId>,
    handles: CompactionHandles,
    policy_constraints: Vec<String>,
    read_hashes: Vec<ReadHash>,
    blockers: Vec<UnresolvedBlocker>,
    next_step: Option<String>,
}

/// Ledger event emitted with a successful compact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionEvent {
    kind: CompactionEventKind,
    session_id: SessionId,
    source_range: EventRange,
    artifact: ArtifactRef,
}

/// Closed compaction event kind. Unknown wire values fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum CompactionEventKind {
    Compacted,
}

/// Artifact plus the event that records its source range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionEffect {
    artifact: CompactionArtifact,
    event: CompactionEvent,
}

/// Host-owned compactor. Callers cannot inject leases or hidden reasoning.
pub struct Compactor;

/// Typed compact failure. Display never echoes session text or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CompactionError {
    Cancelled,
    Timeout,
    InvalidEventRange,
    InvalidText,
    TooManyDecisions,
    TooManyFiles,
    TooManyEvidence,
    TooManyHandles,
    TooManyReadHashes,
    TooManyBlockers,
    TooManyConstraints,
    TooManyQuestions,
    TooManySymbols,
    ArtifactTooLarge,
    ForbiddenPayload,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

impl EventRange {
    pub fn new(from_seq: u64, to_seq: u64) -> Result<Self, CompactionError> {
        if from_seq > to_seq {
            return Err(CompactionError::InvalidEventRange);
        }
        Ok(Self { from_seq, to_seq })
    }

    pub const fn from_seq(self) -> u64 {
        self.from_seq
    }

    pub const fn to_seq(self) -> u64 {
        self.to_seq
    }
}

impl CompactionDecision {
    pub fn new(
        statement: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self, CompactionError> {
        let statement = statement.into();
        let reason = reason.into();
        reject_text(&statement, DEFAULT_MAX_TEXT_BYTES)?;
        reject_text(&reason, DEFAULT_MAX_TEXT_BYTES)?;
        Ok(Self { statement, reason })
    }

    pub fn statement(&self) -> &str {
        &self.statement
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl ChangedFile {
    pub fn new(path: impl Into<String>, symbols: Vec<String>) -> Result<Self, CompactionError> {
        let path = path.into();
        reject_text(&path, DEFAULT_MAX_TEXT_BYTES)?;
        if symbols.len() > DEFAULT_MAX_SYMBOLS {
            return Err(CompactionError::TooManySymbols);
        }
        for symbol in &symbols {
            reject_text(symbol, DEFAULT_MAX_TEXT_BYTES)?;
        }
        Ok(Self { path, symbols })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn symbols(&self) -> &[String] {
        &self.symbols
    }
}

impl CompactionHandles {
    pub fn new(agents: Vec<AgentId>, jobs: Vec<JobId>) -> Result<Self, CompactionError> {
        if agents.len().saturating_add(jobs.len()) > DEFAULT_MAX_HANDLES {
            return Err(CompactionError::TooManyHandles);
        }
        Ok(Self { agents, jobs })
    }

    pub fn agents(&self) -> &[AgentId] {
        &self.agents
    }

    pub fn jobs(&self) -> &[JobId] {
        &self.jobs
    }
}

impl ReadHash {
    pub fn new(
        locator: impl Into<String>,
        content_hash: ArtifactId,
        stale: bool,
    ) -> Result<Self, CompactionError> {
        let locator = locator.into();
        reject_text(&locator, DEFAULT_MAX_TEXT_BYTES)?;
        Ok(Self {
            locator,
            content_hash,
            stale,
        })
    }

    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn content_hash(&self) -> ArtifactId {
        self.content_hash
    }

    pub const fn stale(&self) -> bool {
        self.stale
    }
}

impl UnresolvedBlocker {
    pub fn new(text: impl Into<String>, uncertain: bool) -> Result<Self, CompactionError> {
        let text = text.into();
        reject_text(&text, DEFAULT_MAX_TEXT_BYTES)?;
        Ok(Self { text, uncertain })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn uncertain(&self) -> bool {
        self.uncertain
    }
}

impl CompactionLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_decisions(mut self, value: usize) -> Self {
        self.max_decisions = value;
        self
    }

    pub fn max_files(mut self, value: usize) -> Self {
        self.max_files = value;
        self
    }

    pub fn max_evidence(mut self, value: usize) -> Self {
        self.max_evidence = value;
        self
    }

    pub fn max_handles(mut self, value: usize) -> Self {
        self.max_handles = value;
        self
    }

    pub fn max_read_hashes(mut self, value: usize) -> Self {
        self.max_read_hashes = value;
        self
    }

    pub fn max_blockers(mut self, value: usize) -> Self {
        self.max_blockers = value;
        self
    }

    pub fn max_constraints(mut self, value: usize) -> Self {
        self.max_constraints = value;
        self
    }

    pub fn max_questions(mut self, value: usize) -> Self {
        self.max_questions = value;
        self
    }

    pub fn max_symbols(mut self, value: usize) -> Self {
        self.max_symbols = value;
        self
    }

    pub fn max_text_bytes(mut self, value: usize) -> Self {
        self.max_text_bytes = value;
        self
    }

    pub fn max_artifact_bytes(mut self, value: usize) -> Self {
        self.max_artifact_bytes = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }
}

impl Default for CompactionLimits {
    fn default() -> Self {
        Self {
            max_decisions: DEFAULT_MAX_DECISIONS,
            max_files: DEFAULT_MAX_FILES,
            max_evidence: DEFAULT_MAX_EVIDENCE,
            max_handles: DEFAULT_MAX_HANDLES,
            max_read_hashes: DEFAULT_MAX_READ_HASHES,
            max_blockers: DEFAULT_MAX_BLOCKERS,
            max_constraints: DEFAULT_MAX_CONSTRAINTS,
            max_questions: DEFAULT_MAX_QUESTIONS,
            max_symbols: DEFAULT_MAX_SYMBOLS,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            timeout: DEFAULT_COMPACT_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl CompactionInput {
    pub fn new(session_id: SessionId, source_range: EventRange) -> Self {
        Self {
            session_id,
            source_range,
            goal: None,
            constraints: Vec::new(),
            unresolved_questions: Vec::new(),
            decisions: Vec::new(),
            files: Vec::new(),
            evidence: Vec::new(),
            handles: CompactionHandles::default(),
            policy_constraints: Vec::new(),
            read_hashes: Vec::new(),
            blockers: Vec::new(),
            next_step: None,
            excluded_literals: Vec::new(),
            limits: CompactionLimits::new(),
        }
    }

    pub fn goal(mut self, snapshot: GoalSnapshot) -> Self {
        self.goal = Some(snapshot);
        self
    }

    pub fn constraint(mut self, text: impl Into<String>) -> Self {
        self.constraints.push(text.into());
        self
    }

    pub fn unresolved_question(mut self, text: impl Into<String>) -> Self {
        self.unresolved_questions.push(text.into());
        self
    }

    pub fn decision(mut self, decision: CompactionDecision) -> Self {
        self.decisions.push(decision);
        self
    }

    pub fn file(mut self, file: ChangedFile) -> Self {
        self.files.push(file);
        self
    }

    pub fn evidence(mut self, id: EvidenceId) -> Self {
        self.evidence.push(id);
        self
    }

    pub fn agent(mut self, id: AgentId) -> Self {
        self.handles.agents.push(id);
        self
    }

    pub fn job(mut self, id: JobId) -> Self {
        self.handles.jobs.push(id);
        self
    }

    pub fn policy_constraint(mut self, text: impl Into<String>) -> Self {
        self.policy_constraints.push(text.into());
        self
    }

    pub fn read_hash(mut self, hash: ReadHash) -> Self {
        self.read_hashes.push(hash);
        self
    }

    pub fn blocker(mut self, blocker: UnresolvedBlocker) -> Self {
        self.blockers.push(blocker);
        self
    }

    pub fn next_step(mut self, text: impl Into<String>) -> Self {
        self.next_step = Some(text.into());
        self
    }

    /// Reject if this exact literal appears in any retained text field.
    pub fn reject_literal(mut self, literal: impl Into<String>) -> Self {
        self.excluded_literals.push(literal.into());
        self
    }

    pub fn limits(mut self, limits: CompactionLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn source_range(&self) -> EventRange {
        self.source_range
    }

    pub fn goal_snapshot(&self) -> Option<&GoalSnapshot> {
        self.goal.as_ref()
    }
}

impl CompactionArtifact {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn source_range(&self) -> EventRange {
        self.source_range
    }

    pub fn goal(&self) -> Option<&GoalSnapshot> {
        self.goal.as_ref()
    }

    pub fn constraints(&self) -> &[String] {
        &self.constraints
    }

    pub fn unresolved_questions(&self) -> &[String] {
        &self.unresolved_questions
    }

    pub fn decisions(&self) -> &[CompactionDecision] {
        &self.decisions
    }

    pub fn files(&self) -> &[ChangedFile] {
        &self.files
    }

    pub fn evidence(&self) -> &[EvidenceId] {
        &self.evidence
    }

    pub fn handles(&self) -> &CompactionHandles {
        &self.handles
    }

    pub fn policy_constraints(&self) -> &[String] {
        &self.policy_constraints
    }

    pub fn read_hashes(&self) -> &[ReadHash] {
        &self.read_hashes
    }

    pub fn blockers(&self) -> &[UnresolvedBlocker] {
        &self.blockers
    }

    pub fn next_step(&self) -> Option<&str> {
        self.next_step.as_deref()
    }
}

impl CompactionEvent {
    pub const fn kind(&self) -> CompactionEventKind {
        self.kind
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn source_range(&self) -> EventRange {
        self.source_range
    }

    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }
}

impl CompactionEffect {
    pub fn artifact(&self) -> &CompactionArtifact {
        &self.artifact
    }

    pub fn event(&self) -> &CompactionEvent {
        &self.event
    }
}

impl CompactionEventKind {
    pub const ALL: &'static [Self] = &[Self::Compacted];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compacted => COMPACTION_EVENT_KIND,
        }
    }
}

/// Summarize session state into a durable continuation record and event.
pub fn compact(input: &CompactionInput) -> Result<CompactionEffect, CompactionError> {
    Compactor::compact(input)
}

impl Compactor {
    pub fn compact(input: &CompactionInput) -> Result<CompactionEffect, CompactionError> {
        let started = Instant::now();
        check_limits(&input.limits, started)?;
        validate_counts(input)?;
        validate_texts(input, started)?;

        let artifact = CompactionArtifact {
            session_id: input.session_id,
            source_range: input.source_range,
            goal: input.goal.clone(),
            constraints: input.constraints.clone(),
            unresolved_questions: input.unresolved_questions.clone(),
            decisions: input.decisions.clone(),
            files: input.files.clone(),
            evidence: input.evidence.clone(),
            handles: input.handles.clone(),
            policy_constraints: input.policy_constraints.clone(),
            read_hashes: input.read_hashes.clone(),
            blockers: input.blockers.clone(),
            next_step: input.next_step.clone(),
        };
        check_limits(&input.limits, started)?;

        let bytes = serde_json::to_vec(&artifact).map_err(|_| CompactionError::ForbiddenPayload)?;
        if bytes.len() > input.limits.max_artifact_bytes {
            return Err(CompactionError::ArtifactTooLarge);
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| CompactionError::ForbiddenPayload)?;
        reject_forbidden_value(&value)?;

        let artifact_ref = ArtifactRef::new(
            ArtifactId::from_bytes(&bytes),
            COMPACTION_MEDIA_TYPE,
            bytes.len() as u64,
            RedactionClass::Project,
        );
        let event = CompactionEvent {
            kind: CompactionEventKind::Compacted,
            session_id: input.session_id,
            source_range: input.source_range,
            artifact: artifact_ref,
        };
        Ok(CompactionEffect { artifact, event })
    }
}

impl CompactionError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidEventRange => "invalid_event_range",
            Self::InvalidText => "invalid_text",
            Self::TooManyDecisions => "too_many_decisions",
            Self::TooManyFiles => "too_many_files",
            Self::TooManyEvidence => "too_many_evidence",
            Self::TooManyHandles => "too_many_handles",
            Self::TooManyReadHashes => "too_many_read_hashes",
            Self::TooManyBlockers => "too_many_blockers",
            Self::TooManyConstraints => "too_many_constraints",
            Self::TooManyQuestions => "too_many_questions",
            Self::TooManySymbols => "too_many_symbols",
            Self::ArtifactTooLarge => "artifact_too_large",
            Self::ForbiddenPayload => "forbidden_payload",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::UnsupportedSchemaVersion => "unsupported_schema_version",
        }
    }

    pub const fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled | Self::Timeout => None,
            Self::ForbiddenPayload => Some(ErrorCode::PolicyDenied),
            Self::InvalidEventRange
            | Self::InvalidText
            | Self::TooManyDecisions
            | Self::TooManyFiles
            | Self::TooManyEvidence
            | Self::TooManyHandles
            | Self::TooManyReadHashes
            | Self::TooManyBlockers
            | Self::TooManyConstraints
            | Self::TooManyQuestions
            | Self::TooManySymbols
            | Self::ArtifactTooLarge
            | Self::UnsupportedSchema
            | Self::UnsupportedSchemaVersion => Some(ErrorCode::ConfigInvalid),
        }
    }
}

impl fmt::Display for CompactionEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for CompactionError {}

impl FromStr for CompactionEventKind {
    type Err = CompactionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            COMPACTION_EVENT_KIND => Ok(Self::Compacted),
            _ => Err(CompactionError::UnsupportedSchema),
        }
    }
}

fn validate_counts(input: &CompactionInput) -> Result<(), CompactionError> {
    let limits = &input.limits;
    if input.decisions.len() > limits.max_decisions {
        return Err(CompactionError::TooManyDecisions);
    }
    if input.files.len() > limits.max_files {
        return Err(CompactionError::TooManyFiles);
    }
    if input.evidence.len() > limits.max_evidence {
        return Err(CompactionError::TooManyEvidence);
    }
    if input
        .handles
        .agents
        .len()
        .saturating_add(input.handles.jobs.len())
        > limits.max_handles
    {
        return Err(CompactionError::TooManyHandles);
    }
    if input.read_hashes.len() > limits.max_read_hashes {
        return Err(CompactionError::TooManyReadHashes);
    }
    if input.blockers.len() > limits.max_blockers {
        return Err(CompactionError::TooManyBlockers);
    }
    if input.constraints.len() > limits.max_constraints
        || input.policy_constraints.len() > limits.max_constraints
    {
        return Err(CompactionError::TooManyConstraints);
    }
    if input.unresolved_questions.len() > limits.max_questions {
        return Err(CompactionError::TooManyQuestions);
    }
    for file in &input.files {
        if file.symbols.len() > limits.max_symbols {
            return Err(CompactionError::TooManySymbols);
        }
    }
    Ok(())
}

fn validate_texts(input: &CompactionInput, started: Instant) -> Result<(), CompactionError> {
    let max = input.limits.max_text_bytes;
    let mut index = 0usize;
    for text in input
        .constraints
        .iter()
        .chain(input.unresolved_questions.iter())
        .chain(input.policy_constraints.iter())
        .chain(input.next_step.iter())
    {
        check_stride(&input.limits, started, index)?;
        index += 1;
        reject_bounded_text(text, max, &input.excluded_literals)?;
    }
    for decision in &input.decisions {
        check_stride(&input.limits, started, index)?;
        index += 1;
        reject_bounded_text(&decision.statement, max, &input.excluded_literals)?;
        reject_bounded_text(&decision.reason, max, &input.excluded_literals)?;
    }
    for file in &input.files {
        check_stride(&input.limits, started, index)?;
        index += 1;
        reject_bounded_text(&file.path, max, &input.excluded_literals)?;
        for symbol in &file.symbols {
            reject_bounded_text(symbol, max, &input.excluded_literals)?;
        }
    }
    for hash in &input.read_hashes {
        check_stride(&input.limits, started, index)?;
        index += 1;
        reject_bounded_text(&hash.locator, max, &input.excluded_literals)?;
    }
    for blocker in &input.blockers {
        check_stride(&input.limits, started, index)?;
        index += 1;
        reject_bounded_text(&blocker.text, max, &input.excluded_literals)?;
    }
    if let Some(goal) = &input.goal {
        reject_bounded_text(goal.statement(), max, &input.excluded_literals)?;
        for criterion in goal.completion_criteria() {
            reject_bounded_text(criterion.text(), max, &input.excluded_literals)?;
        }
    }
    Ok(())
}

fn reject_text(text: &str, max_bytes: usize) -> Result<(), CompactionError> {
    reject_bounded_text(text, max_bytes, &[])
}

fn reject_bounded_text(
    text: &str,
    max_bytes: usize,
    excluded: &[String],
) -> Result<(), CompactionError> {
    if text.is_empty() || text.len() > max_bytes {
        return Err(CompactionError::InvalidText);
    }
    if text_smuggles_forbidden(text) {
        return Err(CompactionError::ForbiddenPayload);
    }
    for literal in excluded {
        if !literal.is_empty() && text.contains(literal.as_str()) {
            return Err(CompactionError::ForbiddenPayload);
        }
    }
    Ok(())
}

fn text_smuggles_forbidden(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    FORBIDDEN_KEYS.iter().any(|key| {
        let mut needle = String::with_capacity(key.len() + 2);
        needle.push('"');
        needle.push_str(key);
        needle.push('"');
        lower.contains(&needle)
    })
}

fn is_forbidden_key(key: &str) -> bool {
    FORBIDDEN_KEYS.contains(&key)
}

fn reject_forbidden_value(value: &Value) -> Result<(), CompactionError> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if is_forbidden_key(key) {
                    return Err(CompactionError::ForbiddenPayload);
                }
                reject_forbidden_value(child)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                reject_forbidden_value(item)?;
            }
            Ok(())
        }
        Value::String(text) => {
            if text_smuggles_forbidden(text) {
                Err(CompactionError::ForbiddenPayload)
            } else {
                Ok(())
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
    }
}

fn check_stride(
    limits: &CompactionLimits,
    started: Instant,
    index: usize,
) -> Result<(), CompactionError> {
    if index.is_multiple_of(CANCEL_STRIDE) {
        check_limits(limits, started)?;
    }
    Ok(())
}

fn check_limits(limits: &CompactionLimits, started: Instant) -> Result<(), CompactionError> {
    if limits.cancel.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(CompactionError::Timeout);
    }
    Ok(())
}

impl Serialize for EventRange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("EventRange", RANGE_FIELDS.len())?;
        state.serialize_field("from_seq", &self.from_seq)?;
        state.serialize_field("to_seq", &self.to_seq)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EventRange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            from_seq: u64,
            to_seq: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        EventRange::new(raw.from_seq, raw.to_seq).map_err(de::Error::custom)
    }
}

impl Serialize for CompactionDecision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CompactionDecision", DECISION_FIELDS.len())?;
        state.serialize_field("statement", &self.statement)?;
        state.serialize_field("reason", &self.reason)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CompactionDecision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            statement: String,
            reason: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        CompactionDecision::new(raw.statement, raw.reason).map_err(de::Error::custom)
    }
}

impl Serialize for ChangedFile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ChangedFile", FILE_FIELDS.len())?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("symbols", &self.symbols)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ChangedFile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            path: String,
            symbols: Vec<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        ChangedFile::new(raw.path, raw.symbols).map_err(de::Error::custom)
    }
}

impl Serialize for CompactionHandles {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CompactionHandles", HANDLES_FIELDS.len())?;
        state.serialize_field("agents", &self.agents)?;
        state.serialize_field("jobs", &self.jobs)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CompactionHandles {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            agents: Vec<AgentId>,
            jobs: Vec<JobId>,
        }
        let raw = Raw::deserialize(deserializer)?;
        CompactionHandles::new(raw.agents, raw.jobs).map_err(de::Error::custom)
    }
}

impl Serialize for ReadHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ReadHash", READ_HASH_FIELDS.len())?;
        state.serialize_field("locator", &self.locator)?;
        state.serialize_field("content_hash", &self.content_hash)?;
        state.serialize_field("stale", &self.stale)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ReadHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            locator: String,
            content_hash: ArtifactId,
            stale: bool,
        }
        let raw = Raw::deserialize(deserializer)?;
        ReadHash::new(raw.locator, raw.content_hash, raw.stale).map_err(de::Error::custom)
    }
}

impl Serialize for UnresolvedBlocker {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("UnresolvedBlocker", BLOCKER_FIELDS.len())?;
        state.serialize_field("text", &self.text)?;
        state.serialize_field("uncertain", &self.uncertain)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for UnresolvedBlocker {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            text: String,
            uncertain: bool,
        }
        let raw = Raw::deserialize(deserializer)?;
        UnresolvedBlocker::new(raw.text, raw.uncertain).map_err(de::Error::custom)
    }
}

impl Serialize for CompactionEventKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CompactionEventKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            COMPACTION_EVENT_KIND => Ok(Self::Compacted),
            other => Err(de::Error::unknown_variant(other, &[COMPACTION_EVENT_KIND])),
        }
    }
}

impl Serialize for CompactionArtifact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CompactionArtifact", ARTIFACT_FIELDS.len())?;
        state.serialize_field("schema", COMPACTION_ARTIFACT_SCHEMA)?;
        state.serialize_field("schema_version", &COMPACTION_SCHEMA_VERSION)?;
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("source_range", &self.source_range)?;
        state.serialize_field("goal", &self.goal)?;
        state.serialize_field("constraints", &self.constraints)?;
        state.serialize_field("unresolved_questions", &self.unresolved_questions)?;
        state.serialize_field("decisions", &self.decisions)?;
        state.serialize_field("files", &self.files)?;
        state.serialize_field("evidence", &self.evidence)?;
        state.serialize_field("handles", &self.handles)?;
        state.serialize_field("policy_constraints", &self.policy_constraints)?;
        state.serialize_field("read_hashes", &self.read_hashes)?;
        state.serialize_field("blockers", &self.blockers)?;
        state.serialize_field("next_step", &self.next_step)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CompactionArtifact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            session_id: SessionId,
            source_range: EventRange,
            goal: Option<GoalSnapshot>,
            constraints: Vec<String>,
            unresolved_questions: Vec<String>,
            decisions: Vec<CompactionDecision>,
            files: Vec<ChangedFile>,
            evidence: Vec<EvidenceId>,
            handles: CompactionHandles,
            policy_constraints: Vec<String>,
            read_hashes: Vec<ReadHash>,
            blockers: Vec<UnresolvedBlocker>,
            next_step: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != COMPACTION_ARTIFACT_SCHEMA {
            return Err(de::Error::custom(CompactionError::UnsupportedSchema));
        }
        if raw.schema_version != COMPACTION_SCHEMA_VERSION {
            return Err(de::Error::custom(CompactionError::UnsupportedSchemaVersion));
        }
        Ok(Self {
            session_id: raw.session_id,
            source_range: raw.source_range,
            goal: raw.goal,
            constraints: raw.constraints,
            unresolved_questions: raw.unresolved_questions,
            decisions: raw.decisions,
            files: raw.files,
            evidence: raw.evidence,
            handles: raw.handles,
            policy_constraints: raw.policy_constraints,
            read_hashes: raw.read_hashes,
            blockers: raw.blockers,
            next_step: raw.next_step,
        })
    }
}

impl Serialize for CompactionEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CompactionEvent", EVENT_FIELDS.len())?;
        state.serialize_field("schema", COMPACTION_EVENT_SCHEMA)?;
        state.serialize_field("schema_version", &COMPACTION_SCHEMA_VERSION)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("source_range", &self.source_range)?;
        state.serialize_field("artifact", &self.artifact)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CompactionEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            kind: CompactionEventKind,
            session_id: SessionId,
            source_range: EventRange,
            artifact: ArtifactRef,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != COMPACTION_EVENT_SCHEMA {
            return Err(de::Error::custom(CompactionError::UnsupportedSchema));
        }
        if raw.schema_version != COMPACTION_SCHEMA_VERSION {
            return Err(de::Error::custom(CompactionError::UnsupportedSchemaVersion));
        }
        Ok(Self {
            kind: raw.kind,
            session_id: raw.session_id,
            source_range: raw.source_range,
            artifact: raw.artifact,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::state::{
        Criterion, EvidenceRequirement, GoalActor, GoalBudget, GoalCommand, GoalSpec, GoalState,
        GoalStateMachine, GoalStopReason, GoalUsage,
    };
    use protocol::{AgentId, EvidenceId, GoalId, JobId};
    use std::str::FromStr;

    const SESSION_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const GOAL_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
    const AGENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const JOB_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
    const EVIDENCE_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789af";
    const SECRET_CANARY: &str = "super-secret-password";
    const READ_HASH: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn parse_id<T: FromStr>(raw: &str) -> T
    where
        T::Err: fmt::Debug,
    {
        raw.parse().expect("id")
    }

    fn session_id() -> SessionId {
        parse_id(SESSION_ID)
    }

    fn range() -> EventRange {
        EventRange::new(10, 42).expect("range")
    }

    fn goal_snapshot() -> GoalSnapshot {
        let spec = GoalSpec::new(
            parse_id::<GoalId>(GOAL_ID),
            "ship auth",
            vec![Criterion::new("c1", "tests pass").expect("criterion")],
            GoalBudget::new(Some(10), Some(100_000), None, None),
            vec![EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec");
        let mut machine = GoalStateMachine::new();
        machine
            .apply(GoalCommand::Create(spec), &GoalActor::Human)
            .expect("create");
        machine
            .snapshot()
            .expect("snapshot")
            .clone()
            .with_usage(GoalUsage::new(3, 1200, 40, 7))
    }

    fn blocked_goal() -> GoalSnapshot {
        let spec = GoalSpec::new(
            parse_id::<GoalId>(GOAL_ID),
            "ship auth",
            vec![Criterion::new("c1", "tests pass").expect("criterion")],
            GoalBudget::new(Some(10), Some(100_000), None, None),
            vec![EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
        )
        .expect("spec");
        let mut machine = GoalStateMachine::new();
        machine
            .apply(GoalCommand::Create(spec), &GoalActor::Human)
            .expect("create");
        machine
            .apply(
                GoalCommand::Block {
                    goal_id: parse_id::<GoalId>(GOAL_ID),
                    budget_exhausted: true,
                },
                &GoalActor::System,
            )
            .expect("block");
        machine.snapshot().expect("snapshot").clone()
    }

    fn full_input() -> CompactionInput {
        CompactionInput::new(session_id(), range())
            .goal(goal_snapshot())
            .constraint("do not rewrite history")
            .unresolved_question("which token store?")
            .decision(
                CompactionDecision::new("use existing hasher", "matches current artifact store")
                    .expect("decision"),
            )
            .file(ChangedFile::new("src/auth.rs", vec!["issue_token".to_owned()]).expect("file"))
            .evidence(parse_id(EVIDENCE_ID))
            .agent(parse_id(AGENT_ID))
            .job(parse_id(JOB_ID))
            .policy_constraint("network=deny")
            .read_hash(ReadHash::new("src/auth.rs:1-40", parse_id(READ_HASH), false).expect("hash"))
            .blocker(UnresolvedBlocker::new("waiting on review", false).expect("blocker"))
            .next_step("add regression test")
    }

    #[test]
    fn compact_writes_artifact_and_event_with_source_range() {
        let effect = compact(&full_input()).expect("compact");
        assert_eq!(effect.artifact().session_id(), session_id());
        assert_eq!(effect.artifact().source_range(), range());
        assert_eq!(effect.event().source_range(), range());
        assert_eq!(effect.event().kind(), CompactionEventKind::Compacted);
        assert_eq!(effect.event().kind().as_str(), COMPACTION_EVENT_KIND);
        assert_eq!(effect.event().session_id(), session_id());
        assert_eq!(effect.event().artifact().media_type, COMPACTION_MEDIA_TYPE);
        assert_eq!(effect.event().artifact().redaction, RedactionClass::Project);
        assert!(effect.event().artifact().bytes > 0);

        let json = serde_json::to_vec(effect.artifact()).expect("bytes");
        assert_eq!(effect.event().artifact().id, ArtifactId::from_bytes(&json));
        assert_eq!(effect.event().artifact().bytes, json.len() as u64);

        let event_json = serde_json::to_string(effect.event()).expect("event");
        let decoded: CompactionEvent = serde_json::from_str(&event_json).expect("decode event");
        assert_eq!(decoded, *effect.event());
        assert!(event_json.contains("\"from_seq\":10"));
        assert!(event_json.contains("\"to_seq\":42"));
    }

    #[test]
    fn artifact_preserves_active_goal_budget_and_blockers() {
        let input = CompactionInput::new(session_id(), range())
            .goal(blocked_goal())
            .blocker(UnresolvedBlocker::new("external review", true).expect("blocker"))
            .next_step("wait for review");
        let effect = compact(&input).expect("compact");
        let goal = effect.artifact().goal().expect("goal");
        assert_eq!(goal.state(), GoalState::Blocked);
        assert_eq!(goal.stop_reason(), Some(GoalStopReason::BudgetExhausted));
        assert_eq!(goal.budget().max_turns(), Some(10));
        assert_eq!(goal.budget().max_tokens(), Some(100_000));
        assert_eq!(goal.statement(), "ship auth");
        assert_eq!(effect.artifact().blockers().len(), 1);
        assert_eq!(effect.artifact().blockers()[0].text(), "external review");
        assert!(effect.artifact().blockers()[0].uncertain());
        assert_eq!(effect.artifact().next_step(), Some("wait for review"));
    }

    #[test]
    fn model_text_cannot_invent_completion() {
        let input = CompactionInput::new(session_id(), range())
            .goal(goal_snapshot())
            .decision(
                CompactionDecision::new("shipped", "model claims the goal is complete")
                    .expect("decision"),
            )
            .next_step("mark the goal complete");
        let effect = compact(&input).expect("compact");
        let goal = effect.artifact().goal().expect("goal");
        assert_eq!(goal.state(), GoalState::Active);
        assert!(goal.stop_reason().is_none());
        let json = serde_json::to_value(effect.artifact()).expect("json");
        assert_eq!(json["goal"]["state"], "active");
        assert!(json.get("completed").is_none());
        assert!(json.get("completion").is_none());
    }

    #[test]
    fn forbidden_fields_and_secret_canaries_are_rejected() {
        assert_eq!(
            CompactionDecision::new(
                "keep going",
                r#"carry {"lease_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","action_hash":"aa"}"#,
            ),
            Err(CompactionError::ForbiddenPayload)
        );
        let smuggled = CompactionInput::new(session_id(), range()).constraint(
            r#"carry {"lease_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","action_hash":"aa"}"#,
        );
        assert_eq!(compact(&smuggled), Err(CompactionError::ForbiddenPayload));

        let cot = CompactionInput::new(session_id(), range())
            .unresolved_question(r#"hidden {"chain_of_thought":"step by step private plan"}"#);
        assert_eq!(compact(&cot), Err(CompactionError::ForbiddenPayload));

        let secret = CompactionInput::new(session_id(), range())
            .constraint(format!("token={SECRET_CANARY}"))
            .reject_literal(SECRET_CANARY);
        assert_eq!(compact(&secret), Err(CompactionError::ForbiddenPayload));
        assert_eq!(
            CompactionError::ForbiddenPayload.code(),
            Some(ErrorCode::PolicyDenied)
        );
        assert_eq!(
            CompactionError::ForbiddenPayload.to_string(),
            "forbidden_payload"
        );
        assert!(
            !CompactionError::ForbiddenPayload
                .to_string()
                .contains(SECRET_CANARY)
        );

        let effect = compact(&full_input()).expect("clean compact");
        let artifact_json = serde_json::to_string(effect.artifact()).expect("artifact json");
        let event_json = serde_json::to_string(effect.event()).expect("event json");
        for key in FORBIDDEN_KEYS {
            assert!(
                !artifact_json.contains(&format!("\"{key}\"")),
                "artifact leaked {key}"
            );
            assert!(
                !event_json.contains(&format!("\"{key}\"")),
                "event leaked {key}"
            );
        }
        assert!(!artifact_json.contains(SECRET_CANARY));
        assert!(!event_json.contains(SECRET_CANARY));

        let mut poisoned = serde_json::to_value(effect.artifact()).expect("value");
        poisoned.as_object_mut().expect("object").insert(
            "chain_of_thought".to_owned(),
            Value::String("private".into()),
        );
        assert!(serde_json::from_value::<CompactionArtifact>(poisoned).is_err());

        let mut lease = serde_json::to_value(effect.artifact()).expect("value");
        lease.as_object_mut().expect("object").insert(
            "capability_lease".to_owned(),
            Value::String("lease-bytes".into()),
        );
        assert!(serde_json::from_value::<CompactionArtifact>(lease).is_err());
    }

    #[test]
    fn artifact_golden_round_trips_and_retains_refs() {
        let effect = compact(&full_input()).expect("compact");
        let json = serde_json::to_string(effect.artifact()).expect("serialize");
        let decoded: CompactionArtifact = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, *effect.artifact());
        assert_eq!(decoded.evidence(), &[parse_id::<EvidenceId>(EVIDENCE_ID)]);
        assert_eq!(decoded.handles().agents(), &[parse_id::<AgentId>(AGENT_ID)]);
        assert_eq!(decoded.handles().jobs(), &[parse_id::<JobId>(JOB_ID)]);
        assert_eq!(decoded.read_hashes()[0].content_hash(), parse_id(READ_HASH));
        assert!(!decoded.read_hashes()[0].stale());
        assert_eq!(decoded.files()[0].path(), "src/auth.rs");
        assert_eq!(decoded.files()[0].symbols(), &["issue_token".to_owned()]);
        assert_eq!(decoded.decisions()[0].statement(), "use existing hasher");
        assert_eq!(
            decoded.constraints(),
            &["do not rewrite history".to_owned()]
        );
        assert_eq!(decoded.policy_constraints(), &["network=deny".to_owned()]);
        assert!(json.contains(COMPACTION_ARTIFACT_SCHEMA));
        assert!(json.contains("\"schema_version\":1"));
        assert!(!json.contains("reasoning_chain"));
        assert!(!json.contains("chain_of_thought"));
    }

    #[test]
    fn cancel_timeout_and_bounds_fail_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            compact(
                &CompactionInput::new(session_id(), range())
                    .limits(CompactionLimits::new().cancellation(cancel))
            ),
            Err(CompactionError::Cancelled)
        );
        assert_eq!(
            compact(
                &CompactionInput::new(session_id(), range())
                    .limits(CompactionLimits::new().timeout(Duration::ZERO))
            ),
            Err(CompactionError::Timeout)
        );
        assert_eq!(
            EventRange::new(9, 8),
            Err(CompactionError::InvalidEventRange)
        );
        assert_eq!(
            compact(
                &CompactionInput::new(session_id(), range())
                    .constraint("one")
                    .constraint("two")
                    .limits(CompactionLimits::new().max_constraints(1))
            ),
            Err(CompactionError::TooManyConstraints)
        );
        assert_eq!(
            compact(&CompactionInput::new(session_id(), range()).constraint("")),
            Err(CompactionError::InvalidText)
        );
        assert_eq!(
            compact(
                &CompactionInput::new(session_id(), range())
                    .next_step("ok")
                    .limits(CompactionLimits::new().max_artifact_bytes(8))
            ),
            Err(CompactionError::ArtifactTooLarge)
        );
        assert_eq!(CompactionError::Cancelled.to_string(), "cancelled");
        assert!(CompactionError::Cancelled.code().is_none());
    }

    #[test]
    fn inverted_range_and_unknown_schema_fail_closed() {
        let effect = compact(&full_input()).expect("compact");
        let mut json = serde_json::to_value(effect.artifact()).expect("value");
        json["schema"] = Value::String("other".into());
        assert!(serde_json::from_value::<CompactionArtifact>(json.clone()).is_err());
        json["schema"] = Value::String(COMPACTION_ARTIFACT_SCHEMA.into());
        json["schema_version"] = Value::from(2);
        assert!(serde_json::from_value::<CompactionArtifact>(json).is_err());
        assert_eq!(
            "session.rewound".parse::<CompactionEventKind>(),
            Err(CompactionError::UnsupportedSchema)
        );
    }
}
