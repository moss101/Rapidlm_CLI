//! Typed evidence store and runtime criterion validators.
//!
//! Completion is a runtime predicate. Model text cannot mark a goal complete.
//! Failed, skipped, or unavailable test evidence never satisfies `test_passed`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use protocol::{AgentId, ArtifactId, ArtifactRef, ErrorCode, EvidenceId, GoalId, SessionId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::agent::model::CancellationToken;
use crate::goal::state::{GoalSnapshot, MAX_CRITERION_TEXT_BYTES};

/// Wire schema name for [`EvidenceRecord`].
pub const EVIDENCE_RECORD_SCHEMA: &str = "rapidlm.evidence_record";

/// Wire schema name for [`CriterionVerdicts`].
pub const CRITERION_VERDICTS_SCHEMA: &str = "rapidlm.criterion_verdicts";

/// Wire schema name for [`CompletionCheck`].
pub const COMPLETION_CHECK_SCHEMA: &str = "rapidlm.completion_check";

/// v1 schema version for evidence records and verdict objects.
pub const EVIDENCE_SCHEMA_VERSION: u16 = 1;

/// Assertion that a test command actually passed.
pub const TEST_PASSED: &str = "test_passed";

/// Maximum evidence records retained by one [`EvidenceStore`].
pub const MAX_EVIDENCE_RECORDS: usize = 256;

/// Maximum UTF-8 bytes accepted in [`EvidenceRecord::assertion`].
pub const MAX_ASSERTION_BYTES: usize = MAX_CRITERION_TEXT_BYTES;

/// Maximum UTF-8 bytes accepted in a subject or source locator.
pub const MAX_SUBJECT_BYTES: usize = MAX_CRITERION_TEXT_BYTES;

/// Maximum UTF-8 bytes accepted in a command/tool name.
pub const MAX_COMMAND_BYTES: usize = MAX_CRITERION_TEXT_BYTES;

const CANCEL_STRIDE: usize = 8;

const RECORD_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "goal_id",
    "criterion_id",
    "kind",
    "assertion",
    "producer",
    "source",
    "observed_at",
    "status",
    "freshness",
    "subject_ref",
    "artifact_ref",
    "command",
    "ledger_ref",
];

const SOURCE_FIELDS: &[&str] = &["hash", "locator"];

const LEDGER_REF_FIELDS: &[&str] = &["session_id", "event_id", "seq"];

const PRODUCER_FIELDS: &[&str] = &["kind", "agent_id"];

const VERDICTS_FIELDS: &[&str] = &["schema", "schema_version", "allowed", "verdicts"];

const VERDICT_FIELDS: &[&str] = &["criterion_id", "satisfied", "reason"];

const COMPLETION_FIELDS: &[&str] = &["schema", "schema_version", "allowed", "results"];

/// Closed evidence kind. Unknown wire values fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[non_exhaustive]
pub enum EvidenceKind {
    Test,
    Build,
    Lint,
    Scan,
    Diff,
    RuntimeObservation,
    UserConfirmation,
    ExternalAttestation,
    ManualReview,
    Status,
    Source,
    Artifact,
    Command,
}

/// Observed proof result. Skipped/error/unavailable are never a pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum EvidenceStatus {
    Passed,
    Failed,
    Skipped,
    Unavailable,
    Error,
}

/// Whether the subject is still the one observed by this record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum EvidenceFreshness {
    Fresh,
    Stale,
}

/// Actor that produced the observation. Required on every record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EvidenceProducer {
    Human,
    System,
    MainAgent { agent_id: AgentId },
    Subagent { agent_id: AgentId },
}

/// Ledger event emitted around record/validate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum EvidenceEventKind {
    Recorded,
    Validated,
    Rejected,
}

/// Why a criterion is not satisfied. Display never echoes assertion text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum CriterionUnsatisfied {
    MissingEvidence,
    FailedStatus,
    SkippedStatus,
    UnavailableStatus,
    ErrorStatus,
    Stale,
    MissingSourceHash,
    MissingProducer,
    MissingArtifact,
    MissingCommand,
    InvalidTestPassed,
    UnknownKind,
    UnbackedEvidence,
    /// The cited ledger event does not exist (wrong session, bad seq, or the
    /// row was never written). Structural, not retryable.
    LedgerEventNotFound,
    /// The cited ledger event exists but is not a kind the gate accepts as
    /// backing (see `BACKING_EVENT_KINDS`). Structural, not retryable.
    LedgerEventWrongKind,
    /// The resolver could not be reached (I/O failure, ledger down). Distinct
    /// from the two reasons above: the citation may be perfectly valid and a
    /// retry could satisfy the criterion once the ledger is reachable again.
    LedgerUnavailable,
}

/// Content-addressed origin of an observation. Hash is required.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EvidenceSource {
    hash: ArtifactId,
    locator: Option<String>,
}

/// Maximum UTF-8 bytes for a wire-form ledger event id (UUID text is 36).
pub const MAX_EVENT_REF_BYTES: usize = 64;

/// Durable ledger location an agent-produced record must cite. The gate
/// resolves it against the real event ledger; a reference that does not
/// resolve (wrong session, missing row, non-tool kind) is not evidence.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EvidenceLedgerRef {
    session_id: SessionId,
    event_id: String,
    seq: u64,
}

impl EvidenceLedgerRef {
    pub fn new(
        session_id: SessionId,
        event_id: impl Into<String>,
        seq: u64,
    ) -> Result<Self, EvidenceError> {
        let event_id = event_id.into();
        if event_id.is_empty() || event_id.len() > MAX_EVENT_REF_BYTES {
            return Err(EvidenceError::InvalidLedgerRef);
        }
        if seq == 0 {
            return Err(EvidenceError::InvalidLedgerRef);
        }
        Ok(Self {
            session_id,
            event_id,
            seq,
        })
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// Why a ledger reference does not back a record. Fail-closed: every
/// resolution failure is equivalent to "not evidence".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackingError {
    NotFound,
    NotABackingEvent,
    Unavailable,
}

impl fmt::Display for BackingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("ledger event not found"),
            Self::NotABackingEvent => f.write_str("ledger event is not a tool result"),
            Self::Unavailable => f.write_str("ledger unavailable"),
        }
    }
}

impl Error for BackingError {}

/// Port over the durable event ledger. Implemented by the composition root,
/// which owns both the evidence service and the ledger.
pub trait BackingResolver: Send + Sync {
    fn resolve(&self, ledger_ref: &EvidenceLedgerRef) -> Result<(), BackingError>;
}

/// Thread-safe resolver handle installed on an [`EvidenceService`].
pub type SharedBackingResolver = std::sync::Arc<dyn BackingResolver>;

/// Create payload for a typed evidence record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceSpec {
    id: EvidenceId,
    goal_id: GoalId,
    criterion_id: Option<String>,
    kind: EvidenceKind,
    assertion: String,
    producer: EvidenceProducer,
    source: EvidenceSource,
    observed_at: u64,
    status: EvidenceStatus,
    freshness: EvidenceFreshness,
    subject_ref: String,
    artifact_ref: Option<ArtifactRef>,
    command: Option<String>,
    ledger_ref: Option<EvidenceLedgerRef>,
}

/// Durable typed proof node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceRecord {
    id: EvidenceId,
    goal_id: GoalId,
    criterion_id: Option<String>,
    kind: EvidenceKind,
    assertion: String,
    producer: EvidenceProducer,
    source: EvidenceSource,
    observed_at: u64,
    status: EvidenceStatus,
    freshness: EvidenceFreshness,
    subject_ref: String,
    artifact_ref: Option<ArtifactRef>,
    command: Option<String>,
    ledger_ref: Option<EvidenceLedgerRef>,
}

/// In-process bounded store of typed evidence.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EvidenceStore {
    records: Vec<EvidenceRecord>,
    by_id: BTreeMap<EvidenceId, usize>,
}

/// Store plus runtime criterion evaluation. Holds the optional ledger
/// resolver used to back agent-produced records.
#[derive(Clone, Default)]
pub struct EvidenceService {
    store: EvidenceStore,
    backing: Option<SharedBackingResolver>,
}

impl fmt::Debug for EvidenceService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvidenceService")
            .field("records", &self.store.len())
            .field("backing_installed", &self.backing.is_some())
            .finish()
    }
}

/// Deterministic criterion/evidence checks. Holds no state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CriterionEvaluator;

/// One criterion's runtime verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CriterionVerdict {
    criterion_id: String,
    satisfied: bool,
    reason: Option<CriterionUnsatisfied>,
}

/// Per-criterion verdicts for a goal snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CriterionVerdicts {
    verdicts: Vec<CriterionVerdict>,
}

/// `can_complete` result: allowed only when every criterion is satisfied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionCheck {
    allowed: bool,
    results: CriterionVerdicts,
}

/// Typed evidence failure. Display never echoes assertion or subject text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceError {
    Cancelled,
    MissingProducer,
    MissingSourceHash,
    InvalidAssertion,
    InvalidSubject,
    InvalidCommand,
    InvalidLocator,
    InvalidCriterion,
    MissingArtifact,
    MissingCommand,
    InvalidTestPassed,
    DuplicateId { id: EvidenceId },
    GoalMismatch { expected: GoalId, found: GoalId },
    EvidenceMissing,
    TooManyRecords { limit: usize },
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
    InvalidLedgerRef,
}

impl EvidenceKind {
    pub const ALL: &'static [Self] = &[
        Self::Test,
        Self::Build,
        Self::Lint,
        Self::Scan,
        Self::Diff,
        Self::RuntimeObservation,
        Self::UserConfirmation,
        Self::ExternalAttestation,
        Self::ManualReview,
        Self::Status,
        Self::Source,
        Self::Artifact,
        Self::Command,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Build => "build",
            Self::Lint => "lint",
            Self::Scan => "scan",
            Self::Diff => "diff",
            Self::RuntimeObservation => "runtime_observation",
            Self::UserConfirmation => "user_confirmation",
            Self::ExternalAttestation => "external_attestation",
            Self::ManualReview => "manual_review",
            Self::Status => "status",
            Self::Source => "source",
            Self::Artifact => "artifact",
            Self::Command => "command",
        }
    }

    pub const fn requires_command(self) -> bool {
        matches!(
            self,
            Self::Test | Self::Build | Self::Lint | Self::Scan | Self::Command
        )
    }

    pub const fn requires_artifact(self) -> bool {
        matches!(
            self,
            Self::Diff | Self::Artifact | Self::ExternalAttestation
        )
    }
}

impl EvidenceStatus {
    pub const ALL: &'static [Self] = &[
        Self::Passed,
        Self::Failed,
        Self::Skipped,
        Self::Unavailable,
        Self::Error,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Unavailable => "unavailable",
            Self::Error => "error",
        }
    }

    pub const fn is_passing(self) -> bool {
        matches!(self, Self::Passed)
    }
}

impl EvidenceFreshness {
    pub const ALL: &'static [Self] = &[Self::Fresh, Self::Stale];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
        }
    }

    pub const fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh)
    }
}

impl EvidenceProducer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::System => "system",
            Self::MainAgent { .. } => "main_agent",
            Self::Subagent { .. } => "subagent",
        }
    }

    pub const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Human | Self::System => None,
            Self::MainAgent { agent_id } | Self::Subagent { agent_id } => Some(agent_id),
        }
    }

    /// Agents cannot vouch for themselves: their records must cite a real
    /// ledger event to satisfy a criterion. Human and System records are
    /// exempt because neither actor can fabricate ledger rows to pass a gate.
    pub const fn requires_ledger_backing(self) -> bool {
        matches!(self, Self::MainAgent { .. } | Self::Subagent { .. })
    }
}

impl EvidenceEventKind {
    pub const ALL: &'static [Self] = &[Self::Recorded, Self::Validated, Self::Rejected];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "evidence.recorded",
            Self::Validated => "evidence.validated",
            Self::Rejected => "evidence.rejected",
        }
    }
}

impl CriterionUnsatisfied {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingEvidence => "missing_evidence",
            Self::FailedStatus => "failed_status",
            Self::SkippedStatus => "skipped_status",
            Self::UnavailableStatus => "unavailable_status",
            Self::ErrorStatus => "error_status",
            Self::Stale => "stale",
            Self::MissingSourceHash => "missing_source_hash",
            Self::MissingProducer => "missing_producer",
            Self::MissingArtifact => "missing_artifact",
            Self::MissingCommand => "missing_command",
            Self::InvalidTestPassed => "invalid_test_passed",
            Self::UnknownKind => "unknown_kind",
            Self::UnbackedEvidence => "unbacked_evidence",
            Self::LedgerEventNotFound => "ledger_event_not_found",
            Self::LedgerEventWrongKind => "ledger_event_wrong_kind",
            Self::LedgerUnavailable => "ledger_unavailable",
        }
    }

    /// Whether a caller might reasonably retry and see this criterion become
    /// satisfied without any new evidence being recorded (e.g. the ledger
    /// becomes reachable again, or a flaky check reruns). Every other reason
    /// requires a genuinely new observation. This never affects the
    /// completion gate itself — `satisfied` stays `false` either way; it only
    /// gives a CLI or automation caller a "try again" vs. "this is final"
    /// signal to act on. See `newtask.md` §2.4.
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::UnavailableStatus | Self::ErrorStatus | Self::LedgerUnavailable
        )
    }
}

impl EvidenceSource {
    pub fn new(hash: ArtifactId) -> Self {
        Self {
            hash,
            locator: None,
        }
    }

    pub fn with_locator(mut self, locator: impl Into<String>) -> Result<Self, EvidenceError> {
        let locator = locator.into();
        if !valid_bounded(&locator, MAX_SUBJECT_BYTES) {
            return Err(EvidenceError::InvalidLocator);
        }
        self.locator = Some(locator);
        Ok(self)
    }

    pub fn hash(&self) -> ArtifactId {
        self.hash
    }

    pub fn locator(&self) -> Option<&str> {
        self.locator.as_deref()
    }
}

impl EvidenceSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: EvidenceId,
        goal_id: GoalId,
        kind: EvidenceKind,
        assertion: impl Into<String>,
        producer: EvidenceProducer,
        source: EvidenceSource,
        status: EvidenceStatus,
        subject_ref: impl Into<String>,
    ) -> Result<Self, EvidenceError> {
        let assertion = assertion.into();
        let subject_ref = subject_ref.into();
        if !valid_bounded(&assertion, MAX_ASSERTION_BYTES) {
            return Err(EvidenceError::InvalidAssertion);
        }
        if !valid_bounded(&subject_ref, MAX_SUBJECT_BYTES) {
            return Err(EvidenceError::InvalidSubject);
        }
        if assertion == TEST_PASSED && kind != EvidenceKind::Test {
            return Err(EvidenceError::InvalidTestPassed);
        }
        Ok(Self {
            id,
            goal_id,
            criterion_id: None,
            kind,
            assertion,
            producer,
            source,
            observed_at: 0,
            status,
            freshness: EvidenceFreshness::Fresh,
            subject_ref,
            artifact_ref: None,
            command: None,
            ledger_ref: None,
        })
    }

    pub fn with_criterion_id(
        mut self,
        criterion_id: impl Into<String>,
    ) -> Result<Self, EvidenceError> {
        let criterion_id = criterion_id.into();
        if !valid_bounded(&criterion_id, MAX_CRITERION_TEXT_BYTES) {
            return Err(EvidenceError::InvalidCriterion);
        }
        self.criterion_id = Some(criterion_id);
        Ok(self)
    }

    pub fn with_observed_at(mut self, observed_at: u64) -> Self {
        self.observed_at = observed_at;
        self
    }

    pub fn with_freshness(mut self, freshness: EvidenceFreshness) -> Self {
        self.freshness = freshness;
        self
    }

    pub fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifact_ref = Some(artifact);
        self
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Result<Self, EvidenceError> {
        let command = command.into();
        if !valid_bounded(&command, MAX_COMMAND_BYTES) {
            return Err(EvidenceError::InvalidCommand);
        }
        self.command = Some(command);
        Ok(self)
    }

    /// Cite the durable ledger event that backs this observation. Required
    /// for agent-produced records; ignored for human/system records.
    pub fn with_ledger_ref(mut self, ledger_ref: EvidenceLedgerRef) -> Self {
        self.ledger_ref = Some(ledger_ref);
        self
    }

    fn finish(self) -> Result<EvidenceRecord, EvidenceError> {
        if self.kind.requires_command() && self.command.is_none() {
            return Err(EvidenceError::MissingCommand);
        }
        if self.kind.requires_artifact() && self.artifact_ref.is_none() {
            return Err(EvidenceError::MissingArtifact);
        }
        Ok(EvidenceRecord {
            id: self.id,
            goal_id: self.goal_id,
            criterion_id: self.criterion_id,
            kind: self.kind,
            assertion: self.assertion,
            producer: self.producer,
            source: self.source,
            observed_at: self.observed_at,
            status: self.status,
            freshness: self.freshness,
            subject_ref: self.subject_ref,
            artifact_ref: self.artifact_ref,
            command: self.command,
            ledger_ref: self.ledger_ref,
        })
    }
}

impl EvidenceRecord {
    pub fn id(&self) -> EvidenceId {
        self.id
    }

    pub fn goal_id(&self) -> GoalId {
        self.goal_id
    }

    pub fn criterion_id(&self) -> Option<&str> {
        self.criterion_id.as_deref()
    }

    pub fn kind(&self) -> EvidenceKind {
        self.kind
    }

    pub fn assertion(&self) -> &str {
        &self.assertion
    }

    pub fn producer(&self) -> EvidenceProducer {
        self.producer
    }

    pub fn source(&self) -> &EvidenceSource {
        &self.source
    }

    pub fn observed_at(&self) -> u64 {
        self.observed_at
    }

    pub fn status(&self) -> EvidenceStatus {
        self.status
    }

    pub fn freshness(&self) -> EvidenceFreshness {
        self.freshness
    }

    pub fn subject_ref(&self) -> &str {
        &self.subject_ref
    }

    pub fn artifact_ref(&self) -> Option<&ArtifactRef> {
        self.artifact_ref.as_ref()
    }

    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    pub fn ledger_ref(&self) -> Option<&EvidenceLedgerRef> {
        self.ledger_ref.as_ref()
    }
}

impl EvidenceStore {
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
            by_id: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn records(&self) -> &[EvidenceRecord] {
        &self.records
    }

    pub fn get(&self, id: EvidenceId) -> Option<&EvidenceRecord> {
        self.by_id.get(&id).map(|&idx| &self.records[idx])
    }

    pub fn record(&mut self, spec: EvidenceSpec) -> Result<&EvidenceRecord, EvidenceError> {
        self.record_with_cancel(spec, &CancellationToken::new())
    }

    pub fn record_with_cancel(
        &mut self,
        spec: EvidenceSpec,
        cancel: &CancellationToken,
    ) -> Result<&EvidenceRecord, EvidenceError> {
        if cancel.is_cancelled() {
            return Err(EvidenceError::Cancelled);
        }
        if self.records.len() >= MAX_EVIDENCE_RECORDS {
            return Err(EvidenceError::TooManyRecords {
                limit: MAX_EVIDENCE_RECORDS,
            });
        }
        if self.by_id.contains_key(&spec.id) {
            return Err(EvidenceError::DuplicateId { id: spec.id });
        }
        let record = spec.finish()?;
        let idx = self.records.len();
        self.records.push(record);
        self.by_id.insert(self.records[idx].id, idx);
        Ok(&self.records[idx])
    }

    /// Mark observations of `subject_ref` stale after a relevant change.
    pub fn invalidate_subject(&mut self, subject_ref: &str) -> usize {
        let mut n = 0;
        for record in &mut self.records {
            if record.subject_ref == subject_ref && record.freshness.is_fresh() {
                record.freshness = EvidenceFreshness::Stale;
                n += 1;
            }
        }
        n
    }

    /// Insert an already-decoded record (bounds-checked). Persistence hosts
    /// restore a validated doc with this; decoding already ran the full spec
    /// validation, so no second pass is performed here.
    pub fn restore(&mut self, record: EvidenceRecord) -> Result<&EvidenceRecord, EvidenceError> {
        if self.records.len() >= MAX_EVIDENCE_RECORDS {
            return Err(EvidenceError::TooManyRecords {
                limit: MAX_EVIDENCE_RECORDS,
            });
        }
        if self.by_id.contains_key(&record.id) {
            return Err(EvidenceError::DuplicateId { id: record.id });
        }
        let idx = self.records.len();
        let id = record.id;
        self.records.push(record);
        self.by_id.insert(id, idx);
        Ok(&self.records[idx])
    }

    fn for_goal(&self, goal_id: GoalId) -> impl Iterator<Item = &EvidenceRecord> {
        self.records.iter().filter(move |r| r.goal_id == goal_id)
    }
}

impl EvidenceService {
    pub const fn new() -> Self {
        Self {
            store: EvidenceStore::new(),
            backing: None,
        }
    }

    /// Install the ledger resolver used to back agent-produced records.
    /// Without a resolver, agent-produced records never satisfy a criterion
    /// (fail closed); human and system records are unaffected.
    pub fn set_backing_resolver(&mut self, backing: SharedBackingResolver) {
        self.backing = Some(backing);
    }

    /// Discard every record, leaving the installed backing resolver (if
    /// any) untouched. For a host re-loading the on-disk evidence doc fresh
    /// under a lock: `restore` rejects a record whose id is already present
    /// (`EvidenceError::DuplicateId`), so reloading onto a non-empty store
    /// would spuriously fail on every record this instance already had —
    /// this clears just the store half of that state, not the whole
    /// service, so a previously-installed resolver survives the reload.
    pub fn reset_store(&mut self) {
        self.store = EvidenceStore::new();
    }

    fn backing(&self) -> Option<&SharedBackingResolver> {
        self.backing.as_ref()
    }

    pub fn store(&self) -> &EvidenceStore {
        &self.store
    }

    pub fn record(&mut self, spec: EvidenceSpec) -> Result<&EvidenceRecord, EvidenceError> {
        self.store.record(spec)
    }

    pub fn record_with_cancel(
        &mut self,
        spec: EvidenceSpec,
        cancel: &CancellationToken,
    ) -> Result<&EvidenceRecord, EvidenceError> {
        self.store.record_with_cancel(spec, cancel)
    }

    pub fn invalidate_subject(&mut self, subject_ref: &str) -> usize {
        self.store.invalidate_subject(subject_ref)
    }

    /// Restore an already-decoded record (bounds-checked). See
    /// [`EvidenceStore::restore`].
    pub fn restore(&mut self, record: EvidenceRecord) -> Result<&EvidenceRecord, EvidenceError> {
        self.store.restore(record)
    }

    /// Check status, source, artifact, command, and ledger backing for `goal`.
    pub fn validate_goal(&self, goal: &GoalSnapshot) -> CriterionVerdicts {
        CriterionEvaluator::evaluate_with_backing(goal, &self.store, self.backing())
    }

    pub fn validate_goal_with_cancel(
        &self,
        goal: &GoalSnapshot,
        cancel: &CancellationToken,
    ) -> Result<CriterionVerdicts, EvidenceError> {
        CriterionEvaluator::evaluate_with_cancel(goal, &self.store, self.backing(), cancel)
    }

    pub fn can_complete(&self, goal: &GoalSnapshot) -> CompletionCheck {
        CompletionCheck::from_verdicts(self.validate_goal(goal))
    }
}

impl CriterionEvaluator {
    pub fn evaluate(goal: &GoalSnapshot, store: &EvidenceStore) -> CriterionVerdicts {
        Self::evaluate_with_backing(goal, store, None)
    }

    /// Same checks, with agent-produced records additionally resolved against
    /// the durable ledger. `None` keeps the fail-closed no-resolver stance.
    pub fn evaluate_with_backing(
        goal: &GoalSnapshot,
        store: &EvidenceStore,
        backing: Option<&SharedBackingResolver>,
    ) -> CriterionVerdicts {
        match Self::evaluate_with_cancel(goal, store, backing, &CancellationToken::new()) {
            Ok(verdicts) => verdicts,
            // Fresh token cannot be cancelled; treat any other error as fail-closed.
            Err(_) => CriterionVerdicts {
                verdicts: goal
                    .completion_criteria()
                    .iter()
                    .map(|c| CriterionVerdict {
                        criterion_id: c.id().to_owned(),
                        satisfied: false,
                        reason: Some(CriterionUnsatisfied::MissingEvidence),
                    })
                    .collect(),
            },
        }
    }

    pub fn evaluate_with_cancel(
        goal: &GoalSnapshot,
        store: &EvidenceStore,
        backing: Option<&SharedBackingResolver>,
        cancel: &CancellationToken,
    ) -> Result<CriterionVerdicts, EvidenceError> {
        if cancel.is_cancelled() {
            return Err(EvidenceError::Cancelled);
        }
        let mut verdicts = Vec::with_capacity(goal.completion_criteria().len());
        for (i, criterion) in goal.completion_criteria().iter().enumerate() {
            if i % CANCEL_STRIDE == 0 && cancel.is_cancelled() {
                return Err(EvidenceError::Cancelled);
            }
            verdicts.push(evaluate_criterion(goal, criterion.id(), store, backing));
        }
        Ok(CriterionVerdicts { verdicts })
    }
}

impl CriterionVerdict {
    pub fn criterion_id(&self) -> &str {
        &self.criterion_id
    }

    pub fn satisfied(&self) -> bool {
        self.satisfied
    }

    pub fn reason(&self) -> Option<CriterionUnsatisfied> {
        self.reason
    }
}

impl CriterionVerdicts {
    pub fn verdicts(&self) -> &[CriterionVerdict] {
        &self.verdicts
    }

    pub fn allowed(&self) -> bool {
        !self.verdicts.is_empty() && self.verdicts.iter().all(|v| v.satisfied)
    }

    /// Turn-level rollup of [`CriterionUnsatisfied::retryable`] (Modbit
    /// `VER-002`/`AGT-025`, `newtask.md` §2.4): true only when completion is
    /// currently blocked and *every* blocking criterion could resolve without
    /// any new evidence — never when nothing is blocked (nothing to retry)
    /// and never when even one blocker needs a genuinely new observation, in
    /// which case retrying alone can't help regardless of the others.
    pub fn retry_advisable(&self) -> bool {
        !self.verdicts.is_empty()
            && !self.allowed()
            && self
                .verdicts
                .iter()
                .filter(|v| !v.satisfied)
                .all(|v| v.reason.is_some_and(CriterionUnsatisfied::retryable))
    }
}

impl CompletionCheck {
    fn from_verdicts(results: CriterionVerdicts) -> Self {
        Self {
            allowed: results.allowed(),
            results,
        }
    }

    pub fn allowed(&self) -> bool {
        self.allowed
    }

    pub fn results(&self) -> &CriterionVerdicts {
        &self.results
    }
}

impl EvidenceError {
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::EvidenceMissing
            | Self::InvalidTestPassed
            | Self::MissingArtifact
            | Self::MissingCommand
            | Self::MissingProducer
            | Self::MissingSourceHash => Some(ErrorCode::GoalEvidenceMissing),
            Self::DuplicateId { .. } | Self::GoalMismatch { .. } => {
                Some(ErrorCode::GoalInvalidTransition)
            }
            Self::InvalidAssertion
            | Self::InvalidSubject
            | Self::InvalidCommand
            | Self::InvalidLocator
            | Self::InvalidCriterion
            | Self::InvalidLedgerRef
            | Self::TooManyRecords { .. }
            | Self::UnknownVariant
            | Self::UnsupportedSchema
            | Self::UnsupportedSchemaVersion => Some(ErrorCode::ConfigInvalid),
        }
    }
}

fn evaluate_criterion(
    goal: &GoalSnapshot,
    criterion_id: &str,
    store: &EvidenceStore,
    backing: Option<&SharedBackingResolver>,
) -> CriterionVerdict {
    let required = required_kinds(goal, criterion_id);
    if required.is_empty() {
        return CriterionVerdict {
            criterion_id: criterion_id.to_owned(),
            satisfied: false,
            reason: Some(CriterionUnsatisfied::MissingEvidence),
        };
    }
    let mut first_fail = None;
    for kind in required {
        match kind {
            None => {
                return CriterionVerdict {
                    criterion_id: criterion_id.to_owned(),
                    satisfied: false,
                    reason: Some(CriterionUnsatisfied::UnknownKind),
                };
            }
            Some(kind) => {
                if let Some(reason) =
                    evaluate_kind(goal.id(), criterion_id, kind, store, backing)
                    && first_fail.is_none()
                {
                    first_fail = Some(reason);
                }
            }
        }
    }
    match first_fail {
        None => CriterionVerdict {
            criterion_id: criterion_id.to_owned(),
            satisfied: true,
            reason: None,
        },
        Some(reason) => CriterionVerdict {
            criterion_id: criterion_id.to_owned(),
            satisfied: false,
            reason: Some(reason),
        },
    }
}

fn required_kinds(goal: &GoalSnapshot, criterion_id: &str) -> Vec<Option<EvidenceKind>> {
    let mut kinds = Vec::new();
    for req in goal.evidence_requirements() {
        if req.criterion_id() != criterion_id {
            continue;
        }
        for kind in req.kinds() {
            kinds.push(kind.parse().ok());
        }
    }
    kinds
}

fn evaluate_kind(
    goal_id: GoalId,
    criterion_id: &str,
    kind: EvidenceKind,
    store: &EvidenceStore,
    backing: Option<&SharedBackingResolver>,
) -> Option<CriterionUnsatisfied> {
    let mut seen = false;
    let mut first_fail = None;
    for record in store.for_goal(goal_id) {
        if !applies_to(record, criterion_id, kind) {
            continue;
        }
        seen = true;
        if let Err(reason) = validate_record(record, kind) {
            if first_fail.is_none() {
                first_fail = Some(reason);
            }
            continue;
        }
        if let Err(reason) = check_backing(record, backing) {
            if first_fail.is_none() {
                first_fail = Some(reason);
            }
            continue;
        }
        return None;
    }
    if !seen {
        Some(CriterionUnsatisfied::MissingEvidence)
    } else {
        Some(first_fail.unwrap_or(CriterionUnsatisfied::MissingEvidence))
    }
}

/// Anti-self-assertion gate: an agent-produced record must cite a real
/// ledger event, resolved through the host-installed resolver. The gate
/// itself never softens on any failure mode — a criterion citing an
/// unresolved reference is `satisfied: false` whether the reference was
/// missing, invalid, or merely unreachable right now. What varies is the
/// *reason* surfaced to the caller: a missing citation or an installed
/// resolver's structural rejection (`LedgerEventNotFound`/`WrongKind`) is
/// final, but a resolver outage (`LedgerUnavailable`) is the one case where
/// re-running the same check later could produce a different verdict with no
/// new evidence at all — see `CriterionUnsatisfied::retryable`.
fn check_backing(
    record: &EvidenceRecord,
    backing: Option<&SharedBackingResolver>,
) -> Result<(), CriterionUnsatisfied> {
    if !record.producer.requires_ledger_backing() {
        return Ok(());
    }
    let ledger_ref = match record.ledger_ref() {
        Some(r) => r,
        None => return Err(CriterionUnsatisfied::UnbackedEvidence),
    };
    let resolver = match backing {
        Some(resolver) => resolver,
        None => return Err(CriterionUnsatisfied::UnbackedEvidence),
    };
    resolver.resolve(ledger_ref).map_err(|err| match err {
        BackingError::NotFound => CriterionUnsatisfied::LedgerEventNotFound,
        BackingError::NotABackingEvent => CriterionUnsatisfied::LedgerEventWrongKind,
        BackingError::Unavailable => CriterionUnsatisfied::LedgerUnavailable,
    })
}

fn applies_to(record: &EvidenceRecord, criterion_id: &str, kind: EvidenceKind) -> bool {
    if record.kind != kind {
        return false;
    }
    match record.criterion_id.as_deref() {
        None => true,
        Some(id) => id == criterion_id,
    }
}

fn validate_record(
    record: &EvidenceRecord,
    required_kind: EvidenceKind,
) -> Result<(), CriterionUnsatisfied> {
    validate_source(record)?;
    validate_producer(record)?;
    validate_status(record, required_kind)?;
    validate_command(record, required_kind)?;
    validate_artifact(record, required_kind)?;
    if !record.freshness.is_fresh() {
        return Err(CriterionUnsatisfied::Stale);
    }
    if (required_kind == EvidenceKind::Test || record.assertion == TEST_PASSED)
        && (record.kind != EvidenceKind::Test
            || record.assertion != TEST_PASSED
            || !record.status.is_passing())
    {
        return Err(CriterionUnsatisfied::InvalidTestPassed);
    }
    Ok(())
}

fn validate_status(
    record: &EvidenceRecord,
    required_kind: EvidenceKind,
) -> Result<(), CriterionUnsatisfied> {
    let _ = required_kind;
    match record.status {
        EvidenceStatus::Passed => Ok(()),
        EvidenceStatus::Failed => Err(CriterionUnsatisfied::FailedStatus),
        EvidenceStatus::Skipped => Err(CriterionUnsatisfied::SkippedStatus),
        EvidenceStatus::Unavailable => Err(CriterionUnsatisfied::UnavailableStatus),
        EvidenceStatus::Error => Err(CriterionUnsatisfied::ErrorStatus),
    }
}

fn validate_source(record: &EvidenceRecord) -> Result<(), CriterionUnsatisfied> {
    // `ArtifactId` cannot be constructed empty; keep the explicit gate.
    let _ = record.source.hash;
    Ok(())
}

fn validate_producer(record: &EvidenceRecord) -> Result<(), CriterionUnsatisfied> {
    let _ = record.producer;
    Ok(())
}

fn validate_command(
    record: &EvidenceRecord,
    required_kind: EvidenceKind,
) -> Result<(), CriterionUnsatisfied> {
    if (required_kind.requires_command() || record.kind.requires_command())
        && record.command.is_none()
    {
        return Err(CriterionUnsatisfied::MissingCommand);
    }
    Ok(())
}

fn validate_artifact(
    record: &EvidenceRecord,
    required_kind: EvidenceKind,
) -> Result<(), CriterionUnsatisfied> {
    if (required_kind.requires_artifact() || record.kind.requires_artifact())
        && record.artifact_ref.is_none()
    {
        return Err(CriterionUnsatisfied::MissingArtifact);
    }
    Ok(())
}

fn valid_bounded(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes
}

impl fmt::Display for EvidenceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EvidenceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EvidenceFreshness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EvidenceProducer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EvidenceEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CriterionUnsatisfied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("evidence operation cancelled"),
            Self::MissingProducer => f.write_str("evidence producer is required"),
            Self::MissingSourceHash => f.write_str("evidence source hash is required"),
            Self::InvalidAssertion => f.write_str("evidence assertion is empty or exceeds bound"),
            Self::InvalidSubject => f.write_str("evidence subject is empty or exceeds bound"),
            Self::InvalidCommand => f.write_str("evidence command is empty or exceeds bound"),
            Self::InvalidLocator => f.write_str("evidence locator is empty or exceeds bound"),
            Self::InvalidCriterion => {
                f.write_str("evidence criterion id is empty or exceeds bound")
            }
            Self::MissingArtifact => f.write_str("evidence artifact reference is required"),
            Self::MissingCommand => f.write_str("evidence command is required"),
            Self::InvalidTestPassed => {
                f.write_str("failed, skipped, or unavailable evidence cannot satisfy test_passed")
            }
            Self::DuplicateId { id } => write!(f, "evidence {id} is already recorded"),
            Self::GoalMismatch { expected, found } => {
                write!(f, "evidence goal {found} does not match {expected}")
            }
            Self::EvidenceMissing => f.write_str("required goal evidence is missing"),
            Self::TooManyRecords { limit } => {
                write!(f, "evidence store exceeds {limit}")
            }
            Self::UnknownVariant => f.write_str("unknown evidence variant"),
            Self::UnsupportedSchema => f.write_str("unsupported evidence schema"),
            Self::UnsupportedSchemaVersion => f.write_str("unsupported evidence schema version"),
            Self::InvalidLedgerRef => f.write_str("ledger reference is empty or exceeds bound"),
        }
    }
}

impl Error for EvidenceError {}

impl FromStr for EvidenceKind {
    type Err = EvidenceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for kind in Self::ALL {
            if kind.as_str() == s {
                return Ok(*kind);
            }
        }
        Err(EvidenceError::UnknownVariant)
    }
}

impl FromStr for EvidenceStatus {
    type Err = EvidenceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for status in Self::ALL {
            if status.as_str() == s {
                return Ok(*status);
            }
        }
        Err(EvidenceError::UnknownVariant)
    }
}

impl FromStr for EvidenceFreshness {
    type Err = EvidenceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for freshness in Self::ALL {
            if freshness.as_str() == s {
                return Ok(*freshness);
            }
        }
        Err(EvidenceError::UnknownVariant)
    }
}

impl Serialize for EvidenceKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EvidenceKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse()
            .map_err(|_| de::Error::unknown_variant(&raw, kind_names()))
    }
}

impl Serialize for EvidenceStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EvidenceStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse()
            .map_err(|_| de::Error::unknown_variant(&raw, status_names()))
    }
}

impl Serialize for EvidenceFreshness {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EvidenceFreshness {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse()
            .map_err(|_| de::Error::unknown_variant(&raw, &["fresh", "stale"]))
    }
}

impl Serialize for EvidenceProducer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fields = match self {
            Self::Human | Self::System => 1,
            Self::MainAgent { .. } | Self::Subagent { .. } => 2,
        };
        let mut state = serializer.serialize_struct("EvidenceProducer", fields)?;
        state.serialize_field("kind", self.as_str())?;
        if let Some(agent_id) = self.agent_id() {
            state.serialize_field("agent_id", &agent_id)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for EvidenceProducer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            kind: String,
            agent_id: Option<AgentId>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match raw.kind.as_str() {
            "human" => {
                if raw.agent_id.is_some() {
                    return Err(de::Error::unknown_field("agent_id", PRODUCER_FIELDS));
                }
                Ok(Self::Human)
            }
            "system" => {
                if raw.agent_id.is_some() {
                    return Err(de::Error::unknown_field("agent_id", PRODUCER_FIELDS));
                }
                Ok(Self::System)
            }
            "main_agent" => Ok(Self::MainAgent {
                agent_id: raw
                    .agent_id
                    .ok_or_else(|| de::Error::missing_field("agent_id"))?,
            }),
            "subagent" => Ok(Self::Subagent {
                agent_id: raw
                    .agent_id
                    .ok_or_else(|| de::Error::missing_field("agent_id"))?,
            }),
            other => Err(de::Error::unknown_variant(
                other,
                &["human", "system", "main_agent", "subagent"],
            )),
        }
    }
}

impl Serialize for EvidenceSource {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("EvidenceSource", SOURCE_FIELDS.len())?;
        state.serialize_field("hash", &self.hash)?;
        state.serialize_field("locator", &self.locator)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EvidenceSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            hash: ArtifactId,
            locator: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut source = EvidenceSource::new(raw.hash);
        if let Some(locator) = raw.locator {
            source = source.with_locator(locator).map_err(de::Error::custom)?;
        }
        Ok(source)
    }
}

impl Serialize for EvidenceLedgerRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state =
            serializer.serialize_struct("EvidenceLedgerRef", LEDGER_REF_FIELDS.len())?;
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("event_id", &self.event_id)?;
        state.serialize_field("seq", &self.seq)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EvidenceLedgerRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            session_id: SessionId,
            event_id: String,
            seq: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.session_id, raw.event_id, raw.seq).map_err(de::Error::custom)
    }
}

impl Serialize for EvidenceRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("EvidenceRecord", RECORD_FIELDS.len())?;
        state.serialize_field("schema", EVIDENCE_RECORD_SCHEMA)?;
        state.serialize_field("schema_version", &EVIDENCE_SCHEMA_VERSION)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("goal_id", &self.goal_id)?;
        state.serialize_field("criterion_id", &self.criterion_id)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("assertion", &self.assertion)?;
        state.serialize_field("producer", &self.producer)?;
        state.serialize_field("source", &self.source)?;
        state.serialize_field("observed_at", &self.observed_at)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("freshness", &self.freshness)?;
        state.serialize_field("subject_ref", &self.subject_ref)?;
        state.serialize_field("artifact_ref", &self.artifact_ref)?;
        state.serialize_field("command", &self.command)?;
        state.serialize_field("ledger_ref", &self.ledger_ref)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EvidenceRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            id: EvidenceId,
            goal_id: GoalId,
            criterion_id: Option<String>,
            kind: EvidenceKind,
            assertion: String,
            producer: EvidenceProducer,
            source: EvidenceSource,
            observed_at: u64,
            status: EvidenceStatus,
            freshness: EvidenceFreshness,
            subject_ref: String,
            artifact_ref: Option<ArtifactRef>,
            command: Option<String>,
            ledger_ref: Option<EvidenceLedgerRef>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != EVIDENCE_RECORD_SCHEMA {
            return Err(de::Error::custom(EvidenceError::UnsupportedSchema));
        }
        if raw.schema_version != EVIDENCE_SCHEMA_VERSION {
            return Err(de::Error::custom(EvidenceError::UnsupportedSchemaVersion));
        }
        let mut spec = EvidenceSpec::new(
            raw.id,
            raw.goal_id,
            raw.kind,
            raw.assertion,
            raw.producer,
            raw.source,
            raw.status,
            raw.subject_ref,
        )
        .map_err(de::Error::custom)?
        .with_observed_at(raw.observed_at)
        .with_freshness(raw.freshness);
        if let Some(criterion_id) = raw.criterion_id {
            spec = spec
                .with_criterion_id(criterion_id)
                .map_err(de::Error::custom)?;
        }
        if let Some(artifact) = raw.artifact_ref {
            spec = spec.with_artifact(artifact);
        }
        if let Some(command) = raw.command {
            spec = spec.with_command(command).map_err(de::Error::custom)?;
        }
        if let Some(ledger_ref) = raw.ledger_ref {
            spec = spec.with_ledger_ref(ledger_ref);
        }
        spec.finish().map_err(de::Error::custom)
    }
}

impl Serialize for CriterionVerdict {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CriterionVerdict", VERDICT_FIELDS.len())?;
        state.serialize_field("criterion_id", &self.criterion_id)?;
        state.serialize_field("satisfied", &self.satisfied)?;
        state.serialize_field("reason", &self.reason.map(|r| r.as_str()))?;
        state.end()
    }
}

impl Serialize for CriterionVerdicts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CriterionVerdicts", VERDICTS_FIELDS.len())?;
        state.serialize_field("schema", CRITERION_VERDICTS_SCHEMA)?;
        state.serialize_field("schema_version", &EVIDENCE_SCHEMA_VERSION)?;
        state.serialize_field("allowed", &self.allowed())?;
        state.serialize_field("verdicts", &self.verdicts)?;
        state.end()
    }
}

impl Serialize for CompletionCheck {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CompletionCheck", COMPLETION_FIELDS.len())?;
        state.serialize_field("schema", COMPLETION_CHECK_SCHEMA)?;
        state.serialize_field("schema_version", &EVIDENCE_SCHEMA_VERSION)?;
        state.serialize_field("allowed", &self.allowed)?;
        state.serialize_field("results", &self.results.verdicts)?;
        state.end()
    }
}

fn kind_names() -> &'static [&'static str] {
    &[
        "test",
        "build",
        "lint",
        "scan",
        "diff",
        "runtime_observation",
        "user_confirmation",
        "external_attestation",
        "manual_review",
        "status",
        "source",
        "artifact",
        "command",
    ]
}

fn status_names() -> &'static [&'static str] {
    &["passed", "failed", "skipped", "unavailable", "error"]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::state::{
        Criterion, EvidenceRequirement, GoalActor, GoalBudget, GoalCommand, GoalSpec,
        GoalStateMachine,
    };
    use protocol::{ArtifactRef, RedactionClass};

    const GOAL_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const EVIDENCE_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
    const AGENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";

    const GOLDEN_RECORD: &str = concat!(
        r#"{"schema":"rapidlm.evidence_record","schema_version":1,"#,
        r#""id":"018f3c8a-7e2b-7a10-8c4d-0123456789ae","#,
        r#""goal_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","#,
        r#""criterion_id":"c1","kind":"test","assertion":"test_passed","#,
        r#""producer":{"kind":"system"},"#,
        r#""source":{"hash":"sha256:31f5eaafcc4c25ba2bae5a484032da391707bcc1ba6494abd1353a70143ad69e","locator":"cargo test"},"#,
        r#""observed_at":1,"status":"passed","freshness":"fresh","#,
        r#""subject_ref":"src/lib.rs","artifact_ref":null,"command":"cargo test","ledger_ref":null}"#
    );

    fn parse_id<T: FromStr>(raw: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        raw.parse().expect("id")
    }

    fn goal_id() -> GoalId {
        parse_id(GOAL_ID)
    }

    fn evidence_id() -> EvidenceId {
        parse_id(EVIDENCE_ID)
    }

    fn source() -> EvidenceSource {
        EvidenceSource::new(ArtifactId::from_bytes(b"rapidlm-evidence-fixture"))
            .with_locator("cargo test")
            .expect("locator")
    }

    /// Stub resolver: only the session below, event "evt-ok", seq 3 resolves.
    const REF_SESSION: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";

    fn ref_session() -> SessionId {
        parse_id(REF_SESSION)
    }

    fn backed_ref() -> EvidenceLedgerRef {
        EvidenceLedgerRef::new(ref_session(), "evt-ok", 3).expect("ledger ref")
    }

    struct StubResolver;

    impl BackingResolver for StubResolver {
        fn resolve(&self, ledger_ref: &EvidenceLedgerRef) -> Result<(), BackingError> {
            if ledger_ref.event_id() == "evt-ok"
                && ledger_ref.seq() == 3
                && ledger_ref.session_id() == ref_session()
            {
                Ok(())
            } else {
                Err(BackingError::NotFound)
            }
        }
    }

    fn passing_test() -> EvidenceSpec {
        EvidenceSpec::new(
            evidence_id(),
            goal_id(),
            EvidenceKind::Test,
            TEST_PASSED,
            EvidenceProducer::System,
            source(),
            EvidenceStatus::Passed,
            "src/lib.rs",
        )
        .expect("spec")
        .with_criterion_id("c1")
        .expect("criterion")
        .with_command("cargo test")
        .expect("command")
        .with_observed_at(1)
    }

    fn goal_with_test_requirement() -> GoalSnapshot {
        let spec = GoalSpec::new(
            goal_id(),
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
        machine.snapshot().expect("snapshot").clone()
    }

    fn goal_with_kinds(kinds: &[&str]) -> GoalSnapshot {
        let spec = GoalSpec::new(
            goal_id(),
            "ship auth",
            vec![Criterion::new("c1", "tests pass").expect("criterion")],
            GoalBudget::new(None, None, None, None),
            vec![
                EvidenceRequirement::new("c1", kinds.iter().map(|k| (*k).to_owned()).collect())
                    .expect("req"),
            ],
        )
        .expect("spec");
        let mut machine = GoalStateMachine::new();
        machine
            .apply(GoalCommand::Create(spec), &GoalActor::Human)
            .expect("create");
        machine.snapshot().expect("snapshot").clone()
    }

    #[test]
    fn record_golden_round_trips() {
        let mut store = EvidenceStore::new();
        let recorded = store.record(passing_test()).expect("record").clone();
        let json = serde_json::to_string(&recorded).expect("serialize");
        assert_eq!(json, GOLDEN_RECORD);
        let decoded: EvidenceRecord = serde_json::from_str(GOLDEN_RECORD).expect("decode");
        assert_eq!(decoded, recorded);
        assert_eq!(decoded.assertion(), TEST_PASSED);
        assert_eq!(decoded.status(), EvidenceStatus::Passed);
        assert_eq!(
            decoded.source().hash(),
            ArtifactId::from_bytes(b"rapidlm-evidence-fixture")
        );
    }

    #[test]
    fn kind_and_status_wire_forms_match_domain() {
        assert_eq!(EvidenceKind::Test.as_str(), "test");
        assert_eq!(
            EvidenceKind::RuntimeObservation.as_str(),
            "runtime_observation"
        );
        assert_eq!(EvidenceStatus::Unavailable.as_str(), "unavailable");
        assert_eq!(
            "scan".parse::<EvidenceKind>().expect("kind"),
            EvidenceKind::Scan
        );
        assert!("pass".parse::<EvidenceStatus>().is_err());
        assert_eq!(EvidenceEventKind::Recorded.as_str(), "evidence.recorded");
        assert_eq!(
            EvidenceError::EvidenceMissing.code(),
            Some(ErrorCode::GoalEvidenceMissing)
        );
    }

    #[test]
    fn source_hash_and_producer_are_required() {
        let spec = EvidenceSpec::new(
            evidence_id(),
            goal_id(),
            EvidenceKind::UserConfirmation,
            "confirmed",
            EvidenceProducer::Human,
            EvidenceSource::new(ArtifactId::from_bytes(b"rapidlm-evidence-fixture")),
            EvidenceStatus::Passed,
            "goal",
        )
        .expect("spec");
        assert!(spec.producer.agent_id().is_none());
        assert_eq!(
            spec.source.hash(),
            ArtifactId::from_bytes(b"rapidlm-evidence-fixture")
        );
        let err = serde_json::from_str::<EvidenceRecord>(
            r#"{"schema":"rapidlm.evidence_record","schema_version":1,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ae","goal_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","criterion_id":null,"kind":"user_confirmation","assertion":"confirmed","producer":{"kind":"human"},"source":{"locator":null},"observed_at":0,"status":"passed","freshness":"fresh","subject_ref":"goal","artifact_ref":null,"command":null}"#,
        )
        .expect_err("missing hash");
        let msg = err.to_string();
        assert!(msg.contains("hash") || msg.contains("missing field"));
    }

    #[test]
    fn failed_skipped_unavailable_test_cannot_satisfy_test_passed() {
        let goal = goal_with_test_requirement();
        for status in [
            EvidenceStatus::Failed,
            EvidenceStatus::Skipped,
            EvidenceStatus::Unavailable,
            EvidenceStatus::Error,
        ] {
            let mut service = EvidenceService::new();
            let spec = EvidenceSpec::new(
                evidence_id(),
                goal_id(),
                EvidenceKind::Test,
                TEST_PASSED,
                EvidenceProducer::System,
                source(),
                status,
                "src/lib.rs",
            )
            .expect("spec")
            .with_criterion_id("c1")
            .expect("criterion")
            .with_command("cargo test")
            .expect("command");
            service.record(spec).expect("record");
            let verdicts = service.validate_goal(&goal);
            assert!(!verdicts.allowed(), "{status} must not complete");
            let verdict = &verdicts.verdicts()[0];
            assert!(!verdict.satisfied());
            assert_ne!(
                verdict.reason(),
                Some(CriterionUnsatisfied::MissingEvidence)
            );
            assert_ne!(
                verdict.reason(),
                None,
                "{status} must explain why test_passed failed"
            );
        }
    }

    #[test]
    fn passing_test_with_source_and_command_satisfies_goal() {
        let goal = goal_with_test_requirement();
        let mut service = EvidenceService::new();
        service.record(passing_test()).expect("record");
        let verdicts = service.validate_goal(&goal);
        assert!(verdicts.allowed());
        assert!(verdicts.verdicts()[0].satisfied());
        let check = service.can_complete(&goal);
        assert!(check.allowed());
    }

    #[test]
    fn missing_required_evidence_prevents_completion() {
        let goal = goal_with_test_requirement();
        let service = EvidenceService::new();
        let verdicts = service.validate_goal(&goal);
        assert!(!verdicts.allowed());
        assert_eq!(
            verdicts.verdicts()[0].reason(),
            Some(CriterionUnsatisfied::MissingEvidence)
        );
        assert!(!service.can_complete(&goal).allowed());
    }

    #[test]
    fn stale_evidence_is_unsatisfied_until_rerun() {
        let goal = goal_with_test_requirement();
        let mut service = EvidenceService::new();
        service.record(passing_test()).expect("record");
        assert!(service.validate_goal(&goal).allowed());
        assert_eq!(service.invalidate_subject("src/lib.rs"), 1);
        let verdicts = service.validate_goal(&goal);
        assert!(!verdicts.allowed());
        assert_eq!(
            verdicts.verdicts()[0].reason(),
            Some(CriterionUnsatisfied::Stale)
        );
    }

    #[test]
    fn failing_deterministic_test_is_not_overruled_by_visual_evidence() {
        // P6-020/P6-024 (§10/§11): a required deterministic check that FAILS keeps
        // the criterion unsatisfied even when a visual/manual-review evidence
        // record is present and passing. Deterministic evidence outranks visual.
        let goal = goal_with_test_requirement();
        let mut service = EvidenceService::new();
        service
            .record(
                EvidenceSpec::new(
                    evidence_id(),
                    goal_id(),
                    EvidenceKind::Test,
                    TEST_PASSED,
                    EvidenceProducer::System,
                    source(),
                    EvidenceStatus::Failed,
                    "src/lib.rs",
                )
                .expect("spec")
                .with_criterion_id("c1")
                .expect("criterion")
                .with_command("cargo test")
                .expect("command"),
            )
            .expect("record failing test");
        service
            .record(
                EvidenceSpec::new(
                    EvidenceId::new(),
                    goal_id(),
                    EvidenceKind::ManualReview,
                    "reviewer confirmed the UI looks correct",
                    EvidenceProducer::System,
                    source(),
                    EvidenceStatus::Passed,
                    "ui.png",
                )
                .expect("spec")
                .with_criterion_id("c1")
                .expect("criterion"),
            )
            .expect("record visual evidence");
        let check = service.can_complete(&goal);
        assert!(
            !check.allowed(),
            "a failing required deterministic test must not be overruled by visual evidence"
        );
    }

    #[test]
    fn artifact_and_command_kinds_require_typed_fields() {
        let err = EvidenceSpec::new(
            evidence_id(),
            goal_id(),
            EvidenceKind::Test,
            TEST_PASSED,
            EvidenceProducer::System,
            source(),
            EvidenceStatus::Passed,
            "src/lib.rs",
        )
        .expect("spec")
        .finish()
        .expect_err("command required");
        assert_eq!(err, EvidenceError::MissingCommand);

        let err = EvidenceSpec::new(
            evidence_id(),
            goal_id(),
            EvidenceKind::Artifact,
            "artifact_present",
            EvidenceProducer::System,
            EvidenceSource::new(ArtifactId::from_bytes(b"rapidlm-evidence-fixture")),
            EvidenceStatus::Passed,
            "target/proof.bin",
        )
        .expect("spec")
        .finish()
        .expect_err("artifact required");
        assert_eq!(err, EvidenceError::MissingArtifact);
    }

    #[test]
    fn status_source_artifact_command_evidence_can_satisfy() {
        let goal = goal_with_kinds(&["status", "source", "artifact", "command"]);
        let mut service = EvidenceService::new();
        let hash = ArtifactId::from_bytes(b"rapidlm-evidence-fixture");
        let artifact = ArtifactRef::new(hash, "text/plain", 4, RedactionClass::Project);
        service
            .record(
                EvidenceSpec::new(
                    parse_id("018f3c8a-7e2b-7a10-8c4d-0123456789a1"),
                    goal_id(),
                    EvidenceKind::Status,
                    "status_ok",
                    EvidenceProducer::System,
                    EvidenceSource::new(hash),
                    EvidenceStatus::Passed,
                    "goal",
                )
                .expect("status"),
            )
            .expect("record status");
        service
            .record(
                EvidenceSpec::new(
                    parse_id("018f3c8a-7e2b-7a10-8c4d-0123456789a2"),
                    goal_id(),
                    EvidenceKind::Source,
                    "source_hashed",
                    EvidenceProducer::System,
                    EvidenceSource::new(hash),
                    EvidenceStatus::Passed,
                    "src/lib.rs",
                )
                .expect("source"),
            )
            .expect("record source");
        service
            .record(
                EvidenceSpec::new(
                    parse_id("018f3c8a-7e2b-7a10-8c4d-0123456789a3"),
                    goal_id(),
                    EvidenceKind::Artifact,
                    "artifact_present",
                    EvidenceProducer::System,
                    EvidenceSource::new(hash),
                    EvidenceStatus::Passed,
                    "target/proof.bin",
                )
                .expect("artifact")
                .with_artifact(artifact),
            )
            .expect("record artifact");
        service.set_backing_resolver(std::sync::Arc::new(StubResolver));
        service
            .record(
                EvidenceSpec::new(
                    parse_id("018f3c8a-7e2b-7a10-8c4d-0123456789a4"),
                    goal_id(),
                    EvidenceKind::Command,
                    "command_ran",
                    EvidenceProducer::MainAgent {
                        agent_id: parse_id(AGENT_ID),
                    },
                    EvidenceSource::new(hash),
                    EvidenceStatus::Passed,
                    "src/lib.rs",
                )
                .expect("command")
                .with_command("cargo test")
                .expect("cmd")
                .with_ledger_ref(backed_ref()),
            )
            .expect("record command");
        assert!(service.validate_goal(&goal).allowed());
    }

    #[test]
    fn cancelled_record_is_rejected() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut store = EvidenceStore::new();
        let err = store
            .record_with_cancel(passing_test(), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, EvidenceError::Cancelled);
        assert!(err.code().is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn empty_assertion_and_unknown_schema_fail_closed() {
        let err = EvidenceSpec::new(
            evidence_id(),
            goal_id(),
            EvidenceKind::ManualReview,
            "",
            EvidenceProducer::Human,
            EvidenceSource::new(ArtifactId::from_bytes(b"rapidlm-evidence-fixture")),
            EvidenceStatus::Passed,
            "review",
        )
        .expect_err("empty assertion");
        assert_eq!(err, EvidenceError::InvalidAssertion);
        assert_eq!(err.code(), Some(ErrorCode::ConfigInvalid));

        let err = serde_json::from_str::<EvidenceRecord>(
            &GOLDEN_RECORD.replace("rapidlm.evidence_record", "rapidlm.evidence_record.v2"),
        )
        .expect_err("schema");
        assert!(err.to_string().contains("unsupported evidence schema"));
    }

    #[test]
    fn display_does_not_echo_assertion_or_subject() {
        let err = EvidenceError::InvalidAssertion;
        let text = err.to_string();
        assert!(!text.contains(TEST_PASSED));
        assert!(!text.contains("src/lib.rs"));
    }

    fn agent_command_record() -> EvidenceSpec {
        EvidenceSpec::new(
            evidence_id(),
            goal_id(),
            EvidenceKind::Command,
            "command_ran",
            EvidenceProducer::MainAgent {
                agent_id: parse_id(AGENT_ID),
            },
            source(),
            EvidenceStatus::Passed,
            "src/lib.rs",
        )
        .expect("spec")
        .with_criterion_id("c1")
        .expect("criterion")
        .with_command("cargo test")
        .expect("command")
    }

    #[test]
    fn agent_evidence_without_ledger_ref_fails_closed() {
        let goal = goal_with_kinds(&["command"]);
        let mut service = EvidenceService::new();
        service.record(agent_command_record()).expect("record");
        let verdicts = service.validate_goal(&goal);
        assert!(!verdicts.allowed());
        assert_eq!(
            verdicts.verdicts()[0].reason(),
            Some(CriterionUnsatisfied::UnbackedEvidence)
        );
        assert!(!service.can_complete(&goal).allowed());
    }

    #[test]
    fn agent_evidence_without_resolver_fails_closed() {
        let goal = goal_with_kinds(&["command"]);
        let mut service = EvidenceService::new();
        let spec = agent_command_record().with_ledger_ref(backed_ref());
        service.record(spec).expect("record");
        let verdicts = service.validate_goal(&goal);
        assert!(!verdicts.allowed());
        assert_eq!(
            verdicts.verdicts()[0].reason(),
            Some(CriterionUnsatisfied::UnbackedEvidence)
        );
    }

    #[test]
    fn agent_evidence_with_unresolvable_ref_is_rejected() {
        let goal = goal_with_kinds(&["command"]);
        let mut service = EvidenceService::new();
        service.set_backing_resolver(std::sync::Arc::new(StubResolver));
        let stale_ref = EvidenceLedgerRef::new(ref_session(), "evt-gone", 9).expect("ref");
        let spec = agent_command_record().with_ledger_ref(stale_ref);
        service.record(spec).expect("record");
        let verdicts = service.validate_goal(&goal);
        assert!(!verdicts.allowed());
        // Structural rejection: the gate still blocks completion the same as
        // before, but the reason is now specific enough to tell a caller this
        // citation will never resolve (as opposed to a transient outage).
        assert_eq!(
            verdicts.verdicts()[0].reason(),
            Some(CriterionUnsatisfied::LedgerEventNotFound)
        );
        assert!(!verdicts.verdicts()[0].reason().unwrap().retryable());
    }

    /// Resolver stub that always returns one fixed [`BackingError`], to
    /// exercise `check_backing`'s reason mapping independently of
    /// [`StubResolver`]'s exact-match behavior.
    struct AlwaysErrResolver(BackingError);

    impl BackingResolver for AlwaysErrResolver {
        fn resolve(&self, _ledger_ref: &EvidenceLedgerRef) -> Result<(), BackingError> {
            Err(self.0)
        }
    }

    #[test]
    fn backing_error_reasons_map_one_to_one_and_only_unavailable_is_retryable() {
        let cases = [
            (BackingError::NotFound, CriterionUnsatisfied::LedgerEventNotFound),
            (
                BackingError::NotABackingEvent,
                CriterionUnsatisfied::LedgerEventWrongKind,
            ),
            (BackingError::Unavailable, CriterionUnsatisfied::LedgerUnavailable),
        ];
        for (backing_error, expected_reason) in cases {
            let goal = goal_with_kinds(&["command"]);
            let mut service = EvidenceService::new();
            service.set_backing_resolver(std::sync::Arc::new(AlwaysErrResolver(backing_error)));
            let spec = agent_command_record().with_ledger_ref(backed_ref());
            service.record(spec).expect("record");
            let verdicts = service.validate_goal(&goal);
            assert!(!verdicts.allowed(), "{backing_error:?} must still fail closed");
            let reason = verdicts.verdicts()[0].reason();
            assert_eq!(reason, Some(expected_reason), "for {backing_error:?}");
            assert_eq!(
                reason.unwrap().retryable(),
                matches!(backing_error, BackingError::Unavailable),
                "only a resolver outage should be marked retryable, got {backing_error:?}"
            );
        }
    }

    #[test]
    fn retry_advisable_is_false_once_every_criterion_is_satisfied() {
        let goal = goal_with_test_requirement();
        let mut service = EvidenceService::new();
        service.record(passing_test()).expect("record");
        let verdicts = service.validate_goal(&goal);
        assert!(verdicts.allowed());
        assert!(!verdicts.retry_advisable(), "nothing is blocked, nothing to retry");
    }

    #[test]
    fn retry_advisable_tracks_whether_the_single_blocker_is_retryable() {
        let retryable_goal = goal_with_kinds(&["command"]);
        let mut service = EvidenceService::new();
        service.set_backing_resolver(std::sync::Arc::new(AlwaysErrResolver(
            BackingError::Unavailable,
        )));
        let spec = agent_command_record().with_ledger_ref(backed_ref());
        service.record(spec).expect("record");
        let verdicts = service.validate_goal(&retryable_goal);
        assert!(!verdicts.allowed());
        assert!(verdicts.retry_advisable(), "a resolver outage alone should read as retryable");

        let final_goal = goal_with_kinds(&["command"]);
        let mut service = EvidenceService::new();
        service.set_backing_resolver(std::sync::Arc::new(AlwaysErrResolver(
            BackingError::NotFound,
        )));
        let spec = agent_command_record().with_ledger_ref(backed_ref());
        service.record(spec).expect("record");
        let verdicts = service.validate_goal(&final_goal);
        assert!(!verdicts.allowed());
        assert!(
            !verdicts.retry_advisable(),
            "a structural rejection needs a new observation, not a rerun"
        );
    }

    #[test]
    fn retry_advisable_requires_every_blocker_to_be_retryable_not_just_one() {
        // Two criteria: c1 blocked by a resolver outage (retryable), c2
        // blocked by missing evidence entirely (not retryable — a genuinely
        // new observation is required). One non-retryable blocker must sink
        // the whole rollup, even with a retryable blocker alongside it.
        let spec = GoalSpec::new(
            goal_id(),
            "ship auth",
            vec![
                Criterion::new("c1", "tests pass").expect("criterion"),
                Criterion::new("c2", "docs updated").expect("criterion"),
            ],
            GoalBudget::new(None, None, None, None),
            vec![
                EvidenceRequirement::new("c1", vec!["command".to_owned()]).expect("req"),
                EvidenceRequirement::new("c2", vec!["command".to_owned()]).expect("req"),
            ],
        )
        .expect("spec");
        let mut machine = GoalStateMachine::new();
        machine
            .apply(GoalCommand::Create(spec), &GoalActor::Human)
            .expect("create");
        let goal = machine.snapshot().expect("snapshot").clone();

        let mut service = EvidenceService::new();
        service.set_backing_resolver(std::sync::Arc::new(AlwaysErrResolver(
            BackingError::Unavailable,
        )));
        // Only c1 gets a (retryable-blocked) record; c2 gets none at all.
        let spec = agent_command_record().with_ledger_ref(backed_ref());
        service.record(spec).expect("record");
        let verdicts = service.validate_goal(&goal);
        assert!(!verdicts.allowed());
        let c2 = verdicts
            .verdicts()
            .iter()
            .find(|v| v.criterion_id() == "c2")
            .expect("c2 verdict");
        assert_eq!(c2.reason(), Some(CriterionUnsatisfied::MissingEvidence));
        assert!(
            !verdicts.retry_advisable(),
            "c2's missing-evidence blocker isn't retryable, so the rollup must not be either"
        );
    }

    #[test]
    fn agent_evidence_with_resolved_ref_satisfies() {
        let goal = goal_with_kinds(&["command"]);
        let mut service = EvidenceService::new();
        service.set_backing_resolver(std::sync::Arc::new(StubResolver));
        let spec = agent_command_record().with_ledger_ref(backed_ref());
        service.record(spec).expect("record");
        assert!(service.validate_goal(&goal).allowed());
        assert!(service.can_complete(&goal).allowed());
    }

    #[test]
    fn human_and_system_records_are_exempt_from_backing() {
        let goal = goal_with_test_requirement();
        // No resolver installed: a System record must still satisfy c1.
        let mut service = EvidenceService::new();
        service.record(passing_test()).expect("record");
        assert!(service.validate_goal(&goal).allowed());

        let confirm_goal = goal_with_kinds(&["user_confirmation"]);
        let human = EvidenceSpec::new(
            parse_id("018f3c8a-7e2b-7a10-8c4d-0123456789a5"),
            goal_id(),
            EvidenceKind::UserConfirmation,
            "confirmed",
            EvidenceProducer::Human,
            source(),
            EvidenceStatus::Passed,
            "goal",
        )
        .expect("spec");
        service.record(human).expect("record");
        assert!(service.validate_goal(&confirm_goal).allowed());
    }

    #[test]
    fn ledger_ref_rejects_empty_overlong_or_zero_seq() {
        assert_eq!(
            EvidenceLedgerRef::new(ref_session(), "", 1),
            Err(EvidenceError::InvalidLedgerRef)
        );
        assert_eq!(
            EvidenceLedgerRef::new(ref_session(), "e".repeat(MAX_EVENT_REF_BYTES + 1), 1),
            Err(EvidenceError::InvalidLedgerRef)
        );
        assert_eq!(
            EvidenceLedgerRef::new(ref_session(), "evt-ok", 0),
            Err(EvidenceError::InvalidLedgerRef)
        );
        let long_ok = "e".repeat(MAX_EVENT_REF_BYTES);
        assert!(EvidenceLedgerRef::new(ref_session(), long_ok, 1).is_ok());
    }

    #[test]
    fn ledger_ref_round_trips_and_old_payloads_still_decode() {
        let goal = goal_with_kinds(&["command"]);
        let mut service = EvidenceService::new();
        let spec = agent_command_record().with_ledger_ref(backed_ref());
        let record = service.record(spec).expect("record").clone();
        let json = serde_json::to_string(&record).expect("serialize");
        assert!(json.contains(r#""ledger_ref":{"session_id":"018f3c8a-7e2b-7a10-8c4d-0123456789ac","event_id":"evt-ok","seq":3}"#));
        let decoded: EvidenceRecord = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.ledger_ref(), Some(&backed_ref()));

        // Additive compatibility: a v1 payload written before the field
        // existed still decodes, with no citation (and thus no backing).
        let legacy = GOLDEN_RECORD.replace(r#","ledger_ref":null}"#, "}");
        let decoded: EvidenceRecord = serde_json::from_str(&legacy).expect("decode legacy");
        assert!(decoded.ledger_ref().is_none());
        let _ = goal;
    }
}
