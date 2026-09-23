//! Canonical event envelope and typed event-kind registry.
//!
//! Wire form matches `data-models/event-schema.md`. `seq` is stored, not
//! assigned, here; timestamps are observational and never used to order a
//! session. Unknown payload fields are retained on the erased envelope and
//! ignored by typed payload readers.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use protocol::{AgentId, EventId, IdParseError, RedactionClass, SessionId, TraceId};
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Envelope schema version written on every constructed event.
pub const EVENT_ENVELOPE_SCHEMA: u16 = 1;

/// Maximum UTF-8 bytes accepted in an optional actor `org_id` / `device_id`.
pub const MAX_ACTOR_ATTR_BYTES: usize = 128;

const ENVELOPE_FIELDS: &[&str] = &[
    "schema",
    "event_id",
    "session_id",
    "seq",
    "recorded_at",
    "actor",
    "trace_id",
    "kind",
    "redaction",
    "payload",
];

const ACTOR_FIELDS: &[&str] = &["kind", "id", "org_id", "device_id"];

/// Event envelope parameterized by payload type.
#[derive(Clone, Debug, PartialEq)]
pub struct EventEnvelope<P> {
    schema: u16,
    event_id: EventId,
    session_id: SessionId,
    seq: u64,
    recorded_at: RecordedAt,
    actor: ActorRef,
    trace_id: TraceId,
    kind: EventKind,
    redaction: RedactionClass,
    payload: P,
}

/// Ledger/export form: payload is a JSON value so unknown fields are retained.
pub type ErasedEventEnvelope = EventEnvelope<Value>;

/// UTC observational timestamp. Wire form is RFC3339 with a `Z` suffix.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RecordedAt {
    rfc3339: String,
}

/// Parse failure for a non-canonical `recorded_at` timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedAtParseError;

/// Actor that produced an event. Wire form is `{kind, id, org_id?, device_id?}`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ActorRef {
    kind: ActorKind,
    id: String,
    org_id: Option<String>,
    device_id: Option<String>,
}

/// Registered actor kinds used in event attribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ActorKind {
    Agent,
    Human,
    ExternalHuman,
    System,
}

/// Parse failure for an unknown actor kind string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActorKindParseError;

/// Failure when constructing an [`ActorRef`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorRefError {
    InvalidId,
    EmptyAttribute,
    AttributeTooLong,
}

/// High-level family for a registered [`EventKind`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EventFamily {
    Session,
    Turn,
    Model,
    Tool,
    Approval,
    Goal,
    Agent,
    Workspace,
    Job,
    Context,
    Evidence,
    Security,
    Artifact,
    Handoff,
    Control,
    Knowledge,
    Playbook,
    Automation,
    Trajectory,
    Insights,
    Computer,
    Orchestration,
    Graph,
    Hook,
}

/// Parse failure for an unregistered event kind string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventKindParseError;

/// Failure when constructing, erasing, or decoding an envelope.
#[derive(Debug)]
pub enum EventEnvelopeError {
    UnsupportedSchema { found: u16 },
    Payload(serde_json::Error),
}

macro_rules! define_event_kinds {
    ($($variant:ident = $wire:literal => $family:ident),+ $(,)?) => {
        /// Closed registry of ledger event kinds (`data-models/event-schema.md`).
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        #[non_exhaustive]
        pub enum EventKind {
            $($variant,)+
        }

        impl EventKind {
            /// Registry order: v1 families, then additive v2 kinds.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }

            pub const fn family(self) -> EventFamily {
                match self {
                    $(Self::$variant => EventFamily::$family,)+
                }
            }
        }
    };
}

define_event_kinds! {
    SessionCreated = "session.created" => Session,
    SessionRecovered = "session.recovered" => Session,
    SessionForked = "session.forked" => Session,
    SessionClosed = "session.closed" => Session,
    TurnStarted = "turn.started" => Turn,
    TurnInterrupted = "turn.interrupted" => Turn,
    TurnCompleted = "turn.completed" => Turn,
    TurnFailed = "turn.failed" => Turn,
    MessageQueued = "message.queued" => Turn,
    MessageState = "message.state" => Turn,
    ModelRequested = "model.requested" => Model,
    ModelStreamDelta = "model.stream_delta" => Model,
    ModelCompleted = "model.completed" => Model,
    ModelFailed = "model.failed" => Model,
    ToolRequested = "tool.requested" => Tool,
    ToolAuthorized = "tool.authorized" => Tool,
    ToolApprovalRequired = "tool.approval_required" => Tool,
    ToolStarted = "tool.started" => Tool,
    ToolCompleted = "tool.completed" => Tool,
    ToolFailed = "tool.failed" => Tool,
    ToolDenied = "tool.denied" => Tool,
    ToolContextRequired = "tool.context_required" => Tool,
    ApprovalRequested = "approval.requested" => Approval,
    ApprovalResolved = "approval.resolved" => Approval,
    ApprovalExpired = "approval.expired" => Approval,
    GoalCreated = "goal.created" => Goal,
    GoalUpdated = "goal.updated" => Goal,
    GoalBlocked = "goal.blocked" => Goal,
    GoalPaused = "goal.paused" => Goal,
    GoalResumed = "goal.resumed" => Goal,
    GoalCompleted = "goal.completed" => Goal,
    GoalCancelled = "goal.cancelled" => Goal,
    GoalBudgetUpdated = "goal.budget_updated" => Goal,
    AgentSpawned = "agent.spawned" => Agent,
    AgentStarted = "agent.started" => Agent,
    AgentStateChanged = "agent.state_changed" => Agent,
    AgentResult = "agent.result" => Agent,
    AgentCancelled = "agent.cancelled" => Agent,
    WorkspaceViewCreated = "workspace.view_created" => Workspace,
    WorkspaceMutationDetected = "workspace.mutation_detected" => Workspace,
    WorkspacePatchStaged = "workspace.patch_staged" => Workspace,
    WorkspaceTransactionCommitted = "workspace.transaction_committed" => Workspace,
    WorkspaceTransactionRolledBack = "workspace.transaction_rolled_back" => Workspace,
    JobStarted = "job.started" => Job,
    JobOutput = "job.output" => Job,
    JobCompleted = "job.completed" => Job,
    JobOrphanReconciled = "job.orphan_reconciled" => Job,
    ContextIndexed = "context.indexed" => Context,
    ContextRetrieved = "context.retrieved" => Context,
    ContextCompiled = "context.compiled" => Context,
    ContextMemoryWritten = "context.memory_written" => Context,
    ContextCompacted = "context.compacted" => Context,
    EvidenceRecorded = "evidence.recorded" => Evidence,
    EvidenceValidated = "evidence.validated" => Evidence,
    EvidenceRejected = "evidence.rejected" => Evidence,
    SecurityFinding = "security.finding" => Security,
    SecurityScanCompleted = "security.scan_completed" => Security,
    ArtifactCreated = "artifact.created" => Artifact,
    ArtifactRedacted = "artifact.redacted" => Artifact,
    ArtifactExpired = "artifact.expired" => Artifact,
    AgentPoolBackgroundStarted = "agent.pool.background_started" => Agent,
    AgentPoolBackgroundParked = "agent.pool.background_parked" => Agent,
    AgentMailSent = "agent.mail.sent" => Agent,
    AgentMailDropped = "agent.mail.dropped" => Agent,
    AgentTaskEnvelopeCreated = "agent.task_envelope.created" => Agent,
    AgentTrajectorySummaryCreated = "agent.trajectory_summary.created" => Agent,
    HandoffRequested = "handoff.requested" => Handoff,
    HandoffSourceParked = "handoff.source_parked" => Handoff,
    HandoffBundleReady = "handoff.bundle_ready" => Handoff,
    HandoffTargetRestored = "handoff.target_restored" => Handoff,
    HandoffExecutionLeaseCommitted = "handoff.execution_lease_committed" => Handoff,
    HandoffAborted = "handoff.aborted" => Handoff,
    ControlTransferredToHuman = "control.transferred_to_human" => Control,
    ControlTransferredToAgent = "control.transferred_to_agent" => Control,
    KnowledgeProposed = "knowledge.proposed" => Knowledge,
    KnowledgeApproved = "knowledge.approved" => Knowledge,
    KnowledgeDeprecated = "knowledge.deprecated" => Knowledge,
    PlaybookRunStarted = "playbook.run_started" => Playbook,
    PlaybookStepCompleted = "playbook.step_completed" => Playbook,
    AutomationTriggerReceived = "automation.trigger_received" => Automation,
    TrajectoryCollected = "trajectory.collected" => Trajectory,
    TrajectoryGraded = "trajectory.graded" => Trajectory,
    InsightsGenerated = "insights.generated" => Insights,
    ComputerSurfaceCreated = "computer.surface_created" => Computer,
    ComputerObserved = "computer.observed" => Computer,
    ComputerTargetResolved = "computer.target_resolved" => Computer,
    ComputerActionExecuted = "computer.action_executed" => Computer,
    ComputerAssertionCompleted = "computer.assertion_completed" => Computer,
    ComputerRecordingCompleted = "computer.recording_completed" => Computer,
    OrchestrationTaskContractCreated = "orchestration.task_contract_created" => Orchestration,
    OrchestrationDiscoveryCompleted = "orchestration.discovery_completed" => Orchestration,
    OrchestrationPlanCreated = "orchestration.plan_created" => Orchestration,
    OrchestrationImplementationCompleted = "orchestration.implementation_completed" => Orchestration,
    OrchestrationEvidenceCreated = "orchestration.evidence_created" => Orchestration,
    OrchestrationCheckCompleted = "orchestration.check_completed" => Orchestration,
    OrchestrationVerificationCompleted = "orchestration.verification_completed" => Orchestration,
    OrchestrationGapCreated = "orchestration.gap_created" => Orchestration,
    OrchestrationRepairStarted = "orchestration.repair_started" => Orchestration,
    OrchestrationStrategistInvoked = "orchestration.strategist_invoked" => Orchestration,
    OrchestrationTaskVerified = "orchestration.task_verified" => Orchestration,
    OrchestrationTaskAccepted = "orchestration.task_accepted" => Orchestration,
    OrchestrationTaskBlocked = "orchestration.task_blocked" => Orchestration,
    OrchestrationTaskFailed = "orchestration.task_failed" => Orchestration,
    OrchestrationTaskCancelled = "orchestration.task_cancelled" => Orchestration,
    GraphCreated = "graph.created" => Graph,
    GraphRevisionCommitted = "graph.revision_committed" => Graph,
    GraphProposalRejected = "graph.proposal_rejected" => Graph,
    GraphNodeStateChanged = "graph.node_state_changed" => Graph,
    HookDecided = "hook.decided" => Hook,
    HookInputRewritten = "hook.input_rewritten" => Hook,
    HookFailed = "hook.failed" => Hook,
}

impl<P> EventEnvelope<P> {
    /// Construct a schema-v1 envelope. `seq` is caller-supplied.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: EventId,
        session_id: SessionId,
        seq: u64,
        recorded_at: RecordedAt,
        actor: ActorRef,
        trace_id: TraceId,
        kind: EventKind,
        redaction: RedactionClass,
        payload: P,
    ) -> Self {
        Self {
            schema: EVENT_ENVELOPE_SCHEMA,
            event_id,
            session_id,
            seq,
            recorded_at,
            actor,
            trace_id,
            kind,
            redaction,
            payload,
        }
    }

    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn event_id(&self) -> EventId {
        self.event_id
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn recorded_at(&self) -> &RecordedAt {
        &self.recorded_at
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub fn kind(&self) -> EventKind {
        self.kind
    }

    pub fn redaction(&self) -> RedactionClass {
        self.redaction
    }

    pub fn payload(&self) -> &P {
        &self.payload
    }

    pub fn into_payload(self) -> P {
        self.payload
    }

    pub fn map_payload<Q>(self, f: impl FnOnce(P) -> Q) -> EventEnvelope<Q> {
        EventEnvelope {
            schema: self.schema,
            event_id: self.event_id,
            session_id: self.session_id,
            seq: self.seq,
            recorded_at: self.recorded_at,
            actor: self.actor,
            trace_id: self.trace_id,
            kind: self.kind,
            redaction: self.redaction,
            payload: f(self.payload),
        }
    }
}

impl<P: Serialize> EventEnvelope<P> {
    /// Copy metadata onto an erased envelope, serializing the typed payload.
    pub fn erase(&self) -> Result<ErasedEventEnvelope, EventEnvelopeError> {
        let payload = serde_json::to_value(&self.payload)?;
        Ok(EventEnvelope {
            schema: self.schema,
            event_id: self.event_id,
            session_id: self.session_id,
            seq: self.seq,
            recorded_at: self.recorded_at.clone(),
            actor: self.actor.clone(),
            trace_id: self.trace_id,
            kind: self.kind,
            redaction: self.redaction,
            payload,
        })
    }
}

impl ErasedEventEnvelope {
    /// Decode the retained JSON payload. Unknown fields are ignored by `T`.
    pub fn decode_payload<T: DeserializeOwned>(&self) -> Result<T, EventEnvelopeError> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }

    /// Convert to a typed envelope. Extra payload fields are dropped by `T`.
    pub fn try_typed<T: DeserializeOwned>(self) -> Result<EventEnvelope<T>, EventEnvelopeError> {
        let payload = serde_json::from_value(self.payload)?;
        Ok(EventEnvelope {
            schema: self.schema,
            event_id: self.event_id,
            session_id: self.session_id,
            seq: self.seq,
            recorded_at: self.recorded_at,
            actor: self.actor,
            trace_id: self.trace_id,
            kind: self.kind,
            redaction: self.redaction,
            payload,
        })
    }
}

impl<P: Serialize> Serialize for EventEnvelope<P> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("EventEnvelope", 10)?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("event_id", &self.event_id)?;
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("seq", &self.seq)?;
        state.serialize_field("recorded_at", &self.recorded_at)?;
        state.serialize_field("actor", &self.actor)?;
        state.serialize_field("trace_id", &self.trace_id)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("redaction", &self.redaction)?;
        state.serialize_field("payload", &self.payload)?;
        state.end()
    }
}

impl<'de, P: Deserialize<'de>> Deserialize<'de> for EventEnvelope<P> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct(
            "EventEnvelope",
            ENVELOPE_FIELDS,
            EnvelopeVisitor(PhantomData),
        )
    }
}

struct EnvelopeVisitor<P>(PhantomData<fn() -> P>);

#[derive(Clone, Copy)]
enum EnvelopeField {
    Schema,
    EventId,
    SessionId,
    Seq,
    RecordedAt,
    Actor,
    TraceId,
    Kind,
    Redaction,
    Payload,
}

impl EnvelopeField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "schema" => Some(Self::Schema),
            "event_id" => Some(Self::EventId),
            "session_id" => Some(Self::SessionId),
            "seq" => Some(Self::Seq),
            "recorded_at" => Some(Self::RecordedAt),
            "actor" => Some(Self::Actor),
            "trace_id" => Some(Self::TraceId),
            "kind" => Some(Self::Kind),
            "redaction" => Some(Self::Redaction),
            "payload" => Some(Self::Payload),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for EnvelopeField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(EnvelopeFieldVisitor)
    }
}

struct EnvelopeFieldVisitor;

impl Visitor<'_> for EnvelopeFieldVisitor {
    type Value = EnvelopeField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an EventEnvelope field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        EnvelopeField::from_str(value).ok_or_else(|| E::unknown_field(value, ENVELOPE_FIELDS))
    }
}

impl<'de, P: Deserialize<'de>> Visitor<'de> for EnvelopeVisitor<P> {
    type Value = EventEnvelope<P>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a RapidLM event envelope object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut schema = None;
        let mut event_id = None;
        let mut session_id = None;
        let mut seq = None;
        let mut recorded_at = None;
        let mut actor = None;
        let mut trace_id = None;
        let mut kind = None;
        let mut redaction = None;
        let mut payload = None;

        while let Some(field) = access.next_key()? {
            match field {
                EnvelopeField::Schema => assign_once(&mut schema, access.next_value()?, "schema")?,
                EnvelopeField::EventId => {
                    assign_once(&mut event_id, access.next_value()?, "event_id")?;
                }
                EnvelopeField::SessionId => {
                    assign_once(&mut session_id, access.next_value()?, "session_id")?;
                }
                EnvelopeField::Seq => assign_once(&mut seq, access.next_value()?, "seq")?,
                EnvelopeField::RecordedAt => {
                    assign_once(&mut recorded_at, access.next_value()?, "recorded_at")?;
                }
                EnvelopeField::Actor => assign_once(&mut actor, access.next_value()?, "actor")?,
                EnvelopeField::TraceId => {
                    assign_once(&mut trace_id, access.next_value()?, "trace_id")?;
                }
                EnvelopeField::Kind => assign_once(&mut kind, access.next_value()?, "kind")?,
                EnvelopeField::Redaction => {
                    assign_once(&mut redaction, access.next_value()?, "redaction")?;
                }
                EnvelopeField::Payload => {
                    assign_once(&mut payload, access.next_value()?, "payload")?;
                }
            }
        }

        let schema: u16 = schema.ok_or_else(|| de::Error::missing_field("schema"))?;
        if schema != EVENT_ENVELOPE_SCHEMA {
            return Err(de::Error::custom(EventEnvelopeError::UnsupportedSchema {
                found: schema,
            }));
        }

        Ok(EventEnvelope {
            schema,
            event_id: event_id.ok_or_else(|| de::Error::missing_field("event_id"))?,
            session_id: session_id.ok_or_else(|| de::Error::missing_field("session_id"))?,
            seq: seq.ok_or_else(|| de::Error::missing_field("seq"))?,
            recorded_at: recorded_at.ok_or_else(|| de::Error::missing_field("recorded_at"))?,
            actor: actor.ok_or_else(|| de::Error::missing_field("actor"))?,
            trace_id: trace_id.ok_or_else(|| de::Error::missing_field("trace_id"))?,
            kind: kind.ok_or_else(|| de::Error::missing_field("kind"))?,
            redaction: redaction.ok_or_else(|| de::Error::missing_field("redaction"))?,
            payload: payload.ok_or_else(|| de::Error::missing_field("payload"))?,
        })
    }
}

impl RecordedAt {
    pub fn as_str(&self) -> &str {
        &self.rfc3339
    }
}

impl fmt::Display for RecordedAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rfc3339)
    }
}

impl FromStr for RecordedAt {
    type Err = RecordedAtParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_recorded_at(s)
    }
}

impl Serialize for RecordedAt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.rfc3339)
    }
}

impl<'de> Deserialize<'de> for RecordedAt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(RecordedAtVisitor)
    }
}

struct RecordedAtVisitor;

impl Visitor<'_> for RecordedAtVisitor {
    type Value = RecordedAt;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an RFC3339 UTC timestamp")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        value.parse().map_err(E::custom)
    }
}

impl fmt::Display for RecordedAtParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed recorded_at timestamp")
    }
}

impl Error for RecordedAtParseError {}

impl ActorKind {
    pub const ALL: &'static [Self] = &[Self::Agent, Self::Human, Self::ExternalHuman, Self::System];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Human => "human",
            Self::ExternalHuman => "external_human",
            Self::System => "system",
        }
    }
}

impl fmt::Display for ActorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ActorKind {
    type Err = ActorKindParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for kind in Self::ALL {
            if kind.as_str() == s {
                return Ok(*kind);
            }
        }
        Err(ActorKindParseError)
    }
}

impl Serialize for ActorKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ActorKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(ActorKindVisitor)
    }
}

struct ActorKindVisitor;

impl Visitor<'_> for ActorKindVisitor {
    type Value = ActorKind;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an actor kind string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        value.parse().map_err(E::custom)
    }
}

impl fmt::Display for ActorKindParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown actor kind")
    }
}

impl Error for ActorKindParseError {}

impl ActorRef {
    pub fn new(kind: ActorKind, id: &str) -> Result<Self, ActorRefError> {
        Ok(Self {
            kind,
            id: parse_actor_id(id)?,
            org_id: None,
            device_id: None,
        })
    }

    pub fn agent(id: AgentId) -> Self {
        Self {
            kind: ActorKind::Agent,
            id: id.to_string(),
            org_id: None,
            device_id: None,
        }
    }

    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Result<Self, ActorRefError> {
        self.org_id = Some(validate_actor_attr(org_id.into())?);
        Ok(self)
    }

    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Result<Self, ActorRefError> {
        self.device_id = Some(validate_actor_attr(device_id.into())?);
        Ok(self)
    }

    pub fn kind(&self) -> ActorKind {
        self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn org_id(&self) -> Option<&str> {
        self.org_id.as_deref()
    }

    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }
}

impl Serialize for ActorRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let field_count =
            2 + usize::from(self.org_id.is_some()) + usize::from(self.device_id.is_some());
        let mut state = serializer.serialize_struct("ActorRef", field_count)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("id", &self.id)?;
        if let Some(org_id) = &self.org_id {
            state.serialize_field("org_id", org_id)?;
        }
        if let Some(device_id) = &self.device_id {
            state.serialize_field("device_id", device_id)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for ActorRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("ActorRef", ACTOR_FIELDS, ActorRefVisitor)
    }
}

struct ActorRefVisitor;

#[derive(Clone, Copy)]
enum ActorField {
    Kind,
    Id,
    OrgId,
    DeviceId,
}

impl ActorField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "kind" => Some(Self::Kind),
            "id" => Some(Self::Id),
            "org_id" => Some(Self::OrgId),
            "device_id" => Some(Self::DeviceId),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ActorField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(ActorFieldVisitor)
    }
}

struct ActorFieldVisitor;

impl Visitor<'_> for ActorFieldVisitor {
    type Value = ActorField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an ActorRef field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        ActorField::from_str(value).ok_or_else(|| E::unknown_field(value, ACTOR_FIELDS))
    }
}

impl<'de> Visitor<'de> for ActorRefVisitor {
    type Value = ActorRef;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an actor object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut kind = None;
        let mut id = None;
        let mut org_id: Option<String> = None;
        let mut device_id: Option<String> = None;

        while let Some(field) = access.next_key()? {
            match field {
                ActorField::Kind => assign_once(&mut kind, access.next_value()?, "kind")?,
                ActorField::Id => assign_once(&mut id, access.next_value::<String>()?, "id")?,
                ActorField::OrgId => {
                    assign_once(&mut org_id, access.next_value::<String>()?, "org_id")?;
                }
                ActorField::DeviceId => {
                    assign_once(&mut device_id, access.next_value::<String>()?, "device_id")?;
                }
            }
        }

        let kind = kind.ok_or_else(|| de::Error::missing_field("kind"))?;
        let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
        let mut actor = ActorRef::new(kind, &id).map_err(de::Error::custom)?;
        if let Some(org) = org_id {
            actor = actor.with_org_id(org).map_err(de::Error::custom)?;
        }
        if let Some(device) = device_id {
            actor = actor.with_device_id(device).map_err(de::Error::custom)?;
        }
        Ok(actor)
    }
}

impl fmt::Display for ActorRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidId => "actor id must be a lowercase hyphenated UUID",
            Self::EmptyAttribute => "actor org_id/device_id must be non-empty",
            Self::AttributeTooLong => "actor org_id/device_id exceeds the byte bound",
        })
    }
}

impl Error for ActorRefError {}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = EventKindParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for kind in Self::ALL {
            if kind.as_str() == s {
                return Ok(*kind);
            }
        }
        Err(EventKindParseError)
    }
}

impl Serialize for EventKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(EventKindVisitor)
    }
}

struct EventKindVisitor;

impl Visitor<'_> for EventKindVisitor {
    type Value = EventKind;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a registered event kind string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        value.parse().map_err(E::custom)
    }
}

impl fmt::Display for EventKindParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown event kind")
    }
}

impl Error for EventKindParseError {}

impl fmt::Display for EventEnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { found } => {
                write!(
                    f,
                    "unsupported event envelope schema {found} (expected {EVENT_ENVELOPE_SCHEMA})"
                )
            }
            Self::Payload(err) => write!(f, "event payload error: {err}"),
        }
    }
}

impl Error for EventEnvelopeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Payload(err) => Some(err),
            Self::UnsupportedSchema { .. } => None,
        }
    }
}

impl From<serde_json::Error> for EventEnvelopeError {
    fn from(value: serde_json::Error) -> Self {
        Self::Payload(value)
    }
}

fn parse_actor_id(s: &str) -> Result<String, ActorRefError> {
    let id: EventId = s
        .parse()
        .map_err(|_: IdParseError| ActorRefError::InvalidId)?;
    Ok(id.to_string())
}

fn validate_actor_attr(value: String) -> Result<String, ActorRefError> {
    if value.is_empty() {
        return Err(ActorRefError::EmptyAttribute);
    }
    if value.len() > MAX_ACTOR_ATTR_BYTES {
        return Err(ActorRefError::AttributeTooLong);
    }
    Ok(value)
}

fn parse_recorded_at(s: &str) -> Result<RecordedAt, RecordedAtParseError> {
    if s.len() < 20 || s.len() > 40 {
        return Err(RecordedAtParseError);
    }
    let prefix = s.get(..19).ok_or(RecordedAtParseError)?;
    if !is_valid_datetime_prefix(prefix) {
        return Err(RecordedAtParseError);
    }
    let rest = &s[19..];
    if rest == "Z" {
        return Ok(RecordedAt {
            rfc3339: s.to_owned(),
        });
    }
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.strip_suffix('Z').ok_or(RecordedAtParseError)?;
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(RecordedAtParseError);
        }
        return Ok(RecordedAt {
            rfc3339: s.to_owned(),
        });
    }
    if rest == "+00:00" {
        let mut rfc3339 = String::with_capacity(20);
        rfc3339.push_str(prefix);
        rfc3339.push('Z');
        return Ok(RecordedAt { rfc3339 });
    }
    Err(RecordedAtParseError)
}

fn is_valid_datetime_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return false;
    }
    let Some(year) = parse_digits(&b[0..4]) else {
        return false;
    };
    let Some(month) = parse_digits(&b[5..7]) else {
        return false;
    };
    let Some(day) = parse_digits(&b[8..10]) else {
        return false;
    };
    let Some(hour) = parse_digits(&b[11..13]) else {
        return false;
    };
    let Some(minute) = parse_digits(&b[14..16]) else {
        return false;
    };
    let Some(second) = parse_digits(&b[17..19]) else {
        return false;
    };
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    day >= 1 && day <= days_in_month(year, month)
}

fn parse_digits(bytes: &[u8]) -> Option<u32> {
    let mut n = 0u32;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.saturating_mul(10).saturating_add(u32::from(b - b'0'));
    }
    Some(n)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::BTreeSet;

    const EVENT_ID: &str = "019c0000-0000-7000-8000-000000000001";
    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000002";
    const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000003";
    const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000004";
    const RECORDED_AT: &str = "2026-08-14T15:20:04.123Z";

    /// Golden matching the field set and complete values in `event-schema.md`.
    const GOLDEN_ENVELOPE: &str = r#"{"schema":1,"event_id":"019c0000-0000-7000-8000-000000000001","session_id":"019c0000-0000-7000-8000-000000000002","seq":42,"recorded_at":"2026-08-14T15:20:04.123Z","actor":{"kind":"agent","id":"019c0000-0000-7000-8000-000000000003"},"trace_id":"8f000000-0000-7000-8000-000000000004","kind":"tool.completed","redaction":"project","payload":{}}"#;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct ToolCompletedPayload {
        status: String,
    }

    fn golden_envelope() -> ErasedEventEnvelope {
        EventEnvelope::new(
            EVENT_ID.parse().expect("event id"),
            SESSION_ID.parse().expect("session id"),
            42,
            RECORDED_AT.parse().expect("recorded_at"),
            ActorRef::agent(ACTOR_ID.parse().expect("actor id")),
            TRACE_ID.parse().expect("trace id"),
            EventKind::ToolCompleted,
            RedactionClass::Project,
            serde_json::json!({}),
        )
    }

    #[test]
    fn golden_fixture_matches_event_schema() {
        let envelope = golden_envelope();
        let json = serde_json::to_string(&envelope).expect("serialize");
        assert_eq!(json, GOLDEN_ENVELOPE);

        let decoded: ErasedEventEnvelope =
            serde_json::from_str(GOLDEN_ENVELOPE).expect("deserialize golden");
        assert_eq!(decoded, envelope);
        assert_eq!(decoded.schema(), EVENT_ENVELOPE_SCHEMA);
        assert_eq!(decoded.seq(), 42);
        assert_eq!(decoded.recorded_at().as_str(), RECORDED_AT);
        assert_eq!(decoded.actor().kind(), ActorKind::Agent);
        assert_eq!(decoded.actor().id(), ACTOR_ID);
        assert_eq!(decoded.kind(), EventKind::ToolCompleted);
        assert_eq!(decoded.redaction(), RedactionClass::Project);
        assert_eq!(decoded.payload(), &serde_json::json!({}));
        assert_eq!(decoded.kind().as_str(), "tool.completed");
    }

    #[test]
    fn typed_and_erased_envelopes_preserve_metadata() {
        let typed = EventEnvelope::new(
            EVENT_ID.parse().expect("event id"),
            SESSION_ID.parse().expect("session id"),
            42,
            RECORDED_AT.parse().expect("recorded_at"),
            ActorRef::agent(ACTOR_ID.parse().expect("actor id")),
            TRACE_ID.parse().expect("trace id"),
            EventKind::ToolCompleted,
            RedactionClass::Project,
            ToolCompletedPayload {
                status: "ok".to_owned(),
            },
        );
        let erased = typed.erase().expect("erase");
        assert_eq!(erased.schema(), typed.schema());
        assert_eq!(erased.event_id(), typed.event_id());
        assert_eq!(erased.session_id(), typed.session_id());
        assert_eq!(erased.seq(), typed.seq());
        assert_eq!(erased.recorded_at(), typed.recorded_at());
        assert_eq!(erased.actor(), typed.actor());
        assert_eq!(erased.trace_id(), typed.trace_id());
        assert_eq!(erased.kind(), typed.kind());
        assert_eq!(erased.redaction(), typed.redaction());
        assert_eq!(erased.payload()["status"], "ok");

        let round_trip = erased
            .try_typed::<ToolCompletedPayload>()
            .expect("typed decode");
        assert_eq!(round_trip.payload().status, "ok");
    }

    #[test]
    fn unknown_payload_fields_are_retained_and_ignored() {
        let mut extra = serde_json::Map::new();
        extra.insert("status".to_owned(), Value::String("ok".to_owned()));
        extra.insert("future_flag".to_owned(), Value::Bool(true));
        extra.insert("nested".to_owned(), serde_json::json!({"hint":"retain-me"}));
        let erased = EventEnvelope::new(
            EVENT_ID.parse().expect("event id"),
            SESSION_ID.parse().expect("session id"),
            7,
            RECORDED_AT.parse().expect("recorded_at"),
            ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
            TRACE_ID.parse().expect("trace id"),
            EventKind::ToolCompleted,
            RedactionClass::Project,
            Value::Object(extra),
        );

        let json = serde_json::to_string(&erased).expect("serialize extra");
        let reread: ErasedEventEnvelope = serde_json::from_str(&json).expect("retain");
        assert_eq!(reread.payload()["future_flag"], true);
        assert_eq!(reread.payload()["nested"]["hint"], "retain-me");

        let ignored: ToolCompletedPayload = reread.decode_payload().expect("ignore extra");
        assert_eq!(ignored.status, "ok");
    }

    #[test]
    fn event_kind_registry_covers_v1_and_v2_families() {
        let wires: BTreeSet<&str> = EventKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(wires.len(), EventKind::ALL.len());
        for required in [
            "session.created",
            "session.recovered",
            "session.forked",
            "session.closed",
            "turn.started",
            "turn.interrupted",
            "turn.completed",
            "turn.failed",
            "model.requested",
            "model.stream_delta",
            "model.completed",
            "model.failed",
            "tool.requested",
            "tool.authorized",
            "tool.approval_required",
            "tool.started",
            "tool.completed",
            "tool.failed",
            "tool.denied",
            "approval.requested",
            "approval.resolved",
            "approval.expired",
            "goal.created",
            "goal.updated",
            "goal.blocked",
            "goal.paused",
            "goal.resumed",
            "goal.completed",
            "goal.cancelled",
            "goal.budget_updated",
            "agent.spawned",
            "agent.started",
            "agent.state_changed",
            "agent.result",
            "agent.cancelled",
            "workspace.view_created",
            "workspace.mutation_detected",
            "workspace.patch_staged",
            "workspace.transaction_committed",
            "workspace.transaction_rolled_back",
            "job.started",
            "job.output",
            "job.completed",
            "job.orphan_reconciled",
            "context.indexed",
            "context.retrieved",
            "context.compiled",
            "context.memory_written",
            "context.compacted",
            "evidence.recorded",
            "evidence.validated",
            "evidence.rejected",
            "security.finding",
            "security.scan_completed",
            "artifact.created",
            "artifact.redacted",
            "artifact.expired",
            "agent.pool.background_started",
            "agent.pool.background_parked",
            "agent.mail.sent",
            "agent.mail.dropped",
            "agent.task_envelope.created",
            "agent.trajectory_summary.created",
            "handoff.requested",
            "handoff.source_parked",
            "handoff.bundle_ready",
            "handoff.target_restored",
            "handoff.execution_lease_committed",
            "handoff.aborted",
            "control.transferred_to_human",
            "control.transferred_to_agent",
            "knowledge.proposed",
            "knowledge.approved",
            "knowledge.deprecated",
            "playbook.run_started",
            "playbook.step_completed",
            "automation.trigger_received",
            "trajectory.collected",
            "trajectory.graded",
            "insights.generated",
            "computer.surface_created",
            "computer.observed",
            "computer.target_resolved",
            "computer.action_executed",
            "computer.assertion_completed",
            "computer.recording_completed",
        ] {
            assert!(wires.contains(required), "missing kind {required}");
            let kind: EventKind = required.parse().expect("parse required");
            assert_eq!(kind.as_str(), required);
            assert_eq!(
                kind.family().to_string_for_test(),
                required.split('.').next()
            );
        }
        assert!("not.a.kind".parse::<EventKind>().is_err());
        assert!(serde_json::from_str::<EventKind>("\"unknown.kind\"").is_err());
        assert_eq!(
            EventKind::AgentPoolBackgroundStarted.as_str(),
            "agent.pool.background_started"
        );
        assert_eq!(
            EventKind::AgentPoolBackgroundStarted.family(),
            EventFamily::Agent
        );
    }

    impl EventFamily {
        fn to_string_for_test(self) -> Option<&'static str> {
            Some(match self {
                Self::Session => "session",
                Self::Turn => "turn",
                Self::Model => "model",
                Self::Tool => "tool",
                Self::Approval => "approval",
                Self::Goal => "goal",
                Self::Agent => "agent",
                Self::Workspace => "workspace",
                Self::Job => "job",
                Self::Context => "context",
                Self::Evidence => "evidence",
                Self::Security => "security",
                Self::Artifact => "artifact",
                Self::Handoff => "handoff",
                Self::Control => "control",
                Self::Knowledge => "knowledge",
                Self::Playbook => "playbook",
                Self::Automation => "automation",
                Self::Trajectory => "trajectory",
                Self::Insights => "insights",
                Self::Computer => "computer",
                Self::Orchestration => "orchestration",
                Self::Graph => "graph",
                Self::Hook => "hook",
            })
        }
    }

    #[test]
    fn rejects_unknown_envelope_fields_and_schema() {
        let mut obj: serde_json::Map<String, Value> =
            serde_json::from_str(GOLDEN_ENVELOPE).expect("golden object");
        obj.insert("prompt".to_owned(), Value::String("smuggle".to_owned()));
        assert!(serde_json::from_value::<ErasedEventEnvelope>(Value::Object(obj)).is_err());

        let mut bad_schema: serde_json::Map<String, Value> =
            serde_json::from_str(GOLDEN_ENVELOPE).expect("golden object");
        bad_schema.insert("schema".to_owned(), Value::from(2));
        assert!(serde_json::from_value::<ErasedEventEnvelope>(Value::Object(bad_schema)).is_err());
    }

    #[test]
    fn recorded_at_accepts_rfc3339_utc() {
        let ts: RecordedAt = RECORDED_AT.parse().expect("millis");
        assert_eq!(ts.as_str(), RECORDED_AT);
        let whole: RecordedAt = "2026-08-14T15:20:04Z".parse().expect("whole");
        assert_eq!(whole.as_str(), "2026-08-14T15:20:04Z");
        let offset: RecordedAt = "2026-08-14T15:20:04+00:00".parse().expect("offset");
        assert_eq!(offset.as_str(), "2026-08-14T15:20:04Z");
        for bad in [
            "",
            "2026-08-14 15:20:04Z",
            "2026-08-14T15:20:04z",
            "2026-02-30T00:00:00Z",
            "2026-08-14T25:00:00Z",
            "not-a-timestamp",
        ] {
            assert!(bad.parse::<RecordedAt>().is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn actor_kinds_round_trip() {
        for kind in ActorKind::ALL {
            let json = serde_json::to_string(kind).expect("serialize");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            let decoded: ActorKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, *kind);
        }
        assert!("user".parse::<ActorKind>().is_err());
        assert!(ActorRef::new(ActorKind::Human, "not-a-uuid").is_err());
    }
}
