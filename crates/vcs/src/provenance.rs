//! Append-only provenance graph for change attribution.
//!
//! Edges bind goal, evidence, agent, workspace, patch, verification, and
//! commit identities. Nodes are typed IDs or content hashes; mutable display
//! text is rejected at the type boundary. Durable ledger append is the
//! caller's responsibility.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{AgentId, ArtifactId, EvidenceId, GoalId, Id, WorkspaceViewId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

/// Wire schema name for [`ProvenanceEdge`].
pub const PROVENANCE_EDGE_SCHEMA: &str = "rapidlm.provenance_edge";

/// v1 schema version for provenance edges.
pub const PROVENANCE_EDGE_SCHEMA_VERSION: u16 = 1;

/// Maximum edges retained by one [`ProvenanceStore`].
pub const MAX_PROVENANCE_EDGES: usize = 8192;

/// Maximum evidence IDs accepted on one patch attribution.
pub const MAX_EVIDENCE_REFS: usize = 32;

/// Maximum nodes of one kind returned in a patch lineage.
pub const MAX_LINEAGE_NODES: usize = 64;

const CANCEL_STRIDE: usize = 8;
const EDGE_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "from",
    "to",
    "kind",
    "recorded_at",
    "content_hash",
];

/// Marker for a managed-task identity.
pub enum TaskTag {}

/// Marker for a decision-record identity.
pub enum DecisionRecordTag {}

/// Marker for a model-step identity.
pub enum ModelStepTag {}

/// Managed-task identifier. Not a display title.
pub type TaskId = Id<TaskTag>;

/// Decision-record identifier. Not a display rationale.
pub type DecisionRecordId = Id<DecisionRecordTag>;

/// Model-step identifier. Not prompt or hidden reasoning text.
pub type ModelStepId = Id<ModelStepTag>;

/// Typed causal node. Variants carry IDs or content hashes only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ProvenanceNode {
    Goal(GoalId),
    Task(TaskId),
    Evidence(EvidenceId),
    DecisionRecord(DecisionRecordId),
    ChangeSet(ArtifactId),
    Verification(ArtifactId),
    ModelStep(ModelStepId),
    Agent(AgentId),
    Workspace(WorkspaceViewId),
    Patch(ArtifactId),
    Commit(ArtifactId),
    Symbol(ArtifactId),
}

/// Attribution relation. Endpoints are validated when an edge is built.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProvenanceEdgeKind {
    ProducedBy,
    AttributedTo,
    SupportedBy,
    AppliedTo,
    VerifiedBy,
    CommittedAs,
    DerivedFrom,
    ModifiesSymbol,
}

/// UTC observational timestamp. Wire form is RFC3339 with a `Z` suffix.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RecordedAt {
    rfc3339: String,
}

/// One append-only attribution edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenanceEdge {
    from: ProvenanceNode,
    to: ProvenanceNode,
    kind: ProvenanceEdgeKind,
    recorded_at: RecordedAt,
    content_hash: ArtifactId,
}

/// In-process append-only provenance graph.
pub struct ProvenanceStore {
    inner: Mutex<Inner>,
    max_edges: usize,
}

/// Cooperative cancellation for store operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Attribution of an applied patch. Display titles are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchAttribution {
    patch: ArtifactId,
    agent: AgentId,
    evidence: Vec<EvidenceId>,
    goal: Option<GoalId>,
    workspace: Option<WorkspaceViewId>,
    verification: Option<ArtifactId>,
    commit: Option<ArtifactId>,
    change_set: Option<ArtifactId>,
    symbols: Vec<ArtifactId>,
    recorded_at: RecordedAt,
}

/// IDs reachable from a patch. Order is insertion order, de-duplicated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchLineage {
    patch: ArtifactId,
    agents: Vec<AgentId>,
    evidence: Vec<EvidenceId>,
    goals: Vec<GoalId>,
    workspaces: Vec<WorkspaceViewId>,
    verifications: Vec<ArtifactId>,
    commits: Vec<ArtifactId>,
    change_sets: Vec<ArtifactId>,
    symbols: Vec<ArtifactId>,
}

/// Typed provenance failure. Display never echoes IDs, hashes, or timestamps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvenanceError {
    Cancelled,
    BoundExceeded,
    EdgeLimit { limit: usize },
    InvalidEdge,
    InvalidTimestamp,
    PatchNotFound,
    LockPoisoned,
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
}

struct Inner {
    edges: Vec<ProvenanceEdge>,
    by_from: BTreeMap<ProvenanceNode, Vec<usize>>,
    by_to: BTreeMap<ProvenanceNode, Vec<usize>>,
}

impl ProvenanceNode {
    pub const fn kind_str(self) -> &'static str {
        match self {
            Self::Goal(_) => "goal",
            Self::Task(_) => "task",
            Self::Evidence(_) => "evidence",
            Self::DecisionRecord(_) => "decision_record",
            Self::ChangeSet(_) => "change_set",
            Self::Verification(_) => "verification",
            Self::ModelStep(_) => "model_step",
            Self::Agent(_) => "agent",
            Self::Workspace(_) => "workspace",
            Self::Patch(_) => "patch",
            Self::Commit(_) => "commit",
            Self::Symbol(_) => "symbol",
        }
    }

    fn parse(kind: &str, id: &str) -> Result<Self, ProvenanceError> {
        match kind {
            "goal" => Ok(Self::Goal(parse_id(id)?)),
            "task" => Ok(Self::Task(parse_id(id)?)),
            "evidence" => Ok(Self::Evidence(parse_id(id)?)),
            "decision_record" => Ok(Self::DecisionRecord(parse_id(id)?)),
            "change_set" => Ok(Self::ChangeSet(parse_hash(id)?)),
            "verification" => Ok(Self::Verification(parse_hash(id)?)),
            "model_step" => Ok(Self::ModelStep(parse_id(id)?)),
            "agent" => Ok(Self::Agent(parse_id(id)?)),
            "workspace" => Ok(Self::Workspace(parse_id(id)?)),
            "patch" => Ok(Self::Patch(parse_hash(id)?)),
            "commit" => Ok(Self::Commit(parse_hash(id)?)),
            "symbol" => Ok(Self::Symbol(parse_hash(id)?)),
            _ => Err(ProvenanceError::UnknownVariant),
        }
    }

    fn id_string(self) -> String {
        match self {
            Self::Goal(id) => id.to_string(),
            Self::Task(id) => id.to_string(),
            Self::Evidence(id) => id.to_string(),
            Self::DecisionRecord(id) => id.to_string(),
            Self::ChangeSet(id)
            | Self::Verification(id)
            | Self::Patch(id)
            | Self::Commit(id)
            | Self::Symbol(id) => id.to_string(),
            Self::ModelStep(id) => id.to_string(),
            Self::Agent(id) => id.to_string(),
            Self::Workspace(id) => id.to_string(),
        }
    }

    fn as_patch(self) -> Option<ArtifactId> {
        match self {
            Self::Patch(id) => Some(id),
            _ => None,
        }
    }

    fn as_agent(self) -> Option<AgentId> {
        match self {
            Self::Agent(id) => Some(id),
            _ => None,
        }
    }

    fn as_evidence(self) -> Option<EvidenceId> {
        match self {
            Self::Evidence(id) => Some(id),
            _ => None,
        }
    }

    fn as_goal(self) -> Option<GoalId> {
        match self {
            Self::Goal(id) => Some(id),
            _ => None,
        }
    }

    fn as_workspace(self) -> Option<WorkspaceViewId> {
        match self {
            Self::Workspace(id) => Some(id),
            _ => None,
        }
    }

    fn as_verification(self) -> Option<ArtifactId> {
        match self {
            Self::Verification(id) => Some(id),
            _ => None,
        }
    }

    fn as_commit(self) -> Option<ArtifactId> {
        match self {
            Self::Commit(id) => Some(id),
            _ => None,
        }
    }

    fn as_change_set(self) -> Option<ArtifactId> {
        match self {
            Self::ChangeSet(id) => Some(id),
            _ => None,
        }
    }

    fn as_symbol(self) -> Option<ArtifactId> {
        match self {
            Self::Symbol(id) => Some(id),
            _ => None,
        }
    }
}

impl ProvenanceEdgeKind {
    pub const ALL: &'static [Self] = &[
        Self::ProducedBy,
        Self::AttributedTo,
        Self::SupportedBy,
        Self::AppliedTo,
        Self::VerifiedBy,
        Self::CommittedAs,
        Self::DerivedFrom,
        Self::ModifiesSymbol,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProducedBy => "produced_by",
            Self::AttributedTo => "attributed_to",
            Self::SupportedBy => "supported_by",
            Self::AppliedTo => "applied_to",
            Self::VerifiedBy => "verified_by",
            Self::CommittedAs => "committed_as",
            Self::DerivedFrom => "derived_from",
            Self::ModifiesSymbol => "modifies_symbol",
        }
    }
}

impl RecordedAt {
    pub fn as_str(&self) -> &str {
        &self.rfc3339
    }

    pub fn from_unix_secs(secs: u64) -> Result<Self, ProvenanceError> {
        let formatted = unix_secs_to_rfc3339(secs)?;
        parse_recorded_at(&formatted)
    }

    pub fn from_system_time(time: SystemTime) -> Result<Self, ProvenanceError> {
        let secs = time
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProvenanceError::InvalidTimestamp)?
            .as_secs();
        Self::from_unix_secs(secs)
    }
}

impl ProvenanceEdge {
    /// Build an edge after validating kind/endpoints. `content_hash` is the
    /// attributed content, never a display label.
    pub fn new(
        from: ProvenanceNode,
        to: ProvenanceNode,
        kind: ProvenanceEdgeKind,
        recorded_at: RecordedAt,
        content_hash: ArtifactId,
    ) -> Result<Self, ProvenanceError> {
        validate_edge(from, to, kind)?;
        Ok(Self {
            from,
            to,
            kind,
            recorded_at,
            content_hash,
        })
    }

    pub fn from(&self) -> ProvenanceNode {
        self.from
    }

    pub fn to(&self) -> ProvenanceNode {
        self.to
    }

    pub fn kind(&self) -> ProvenanceEdgeKind {
        self.kind
    }

    pub fn recorded_at(&self) -> &RecordedAt {
        &self.recorded_at
    }

    pub fn content_hash(&self) -> ArtifactId {
        self.content_hash
    }
}

impl PatchAttribution {
    pub fn new(patch: ArtifactId, agent: AgentId, recorded_at: RecordedAt) -> Self {
        Self {
            patch,
            agent,
            evidence: Vec::new(),
            goal: None,
            workspace: None,
            verification: None,
            commit: None,
            change_set: None,
            symbols: Vec::new(),
            recorded_at,
        }
    }

    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = EvidenceId>) -> Self {
        self.evidence.extend(evidence);
        self
    }

    pub fn with_goal(mut self, goal: GoalId) -> Self {
        self.goal = Some(goal);
        self
    }

    pub fn with_workspace(mut self, workspace: WorkspaceViewId) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_verification(mut self, verification: ArtifactId) -> Self {
        self.verification = Some(verification);
        self
    }

    pub fn with_commit(mut self, commit: ArtifactId) -> Self {
        self.commit = Some(commit);
        self
    }

    pub fn with_change_set(mut self, change_set: ArtifactId) -> Self {
        self.change_set = Some(change_set);
        self
    }

    pub fn with_symbols(mut self, symbols: impl IntoIterator<Item = ArtifactId>) -> Self {
        self.symbols.extend(symbols);
        self
    }

    pub fn patch(&self) -> ArtifactId {
        self.patch
    }

    pub fn agent(&self) -> AgentId {
        self.agent
    }

    pub fn evidence(&self) -> &[EvidenceId] {
        &self.evidence
    }

    pub fn goal(&self) -> Option<GoalId> {
        self.goal
    }

    pub fn workspace(&self) -> Option<WorkspaceViewId> {
        self.workspace
    }

    pub fn verification(&self) -> Option<ArtifactId> {
        self.verification
    }

    pub fn commit(&self) -> Option<ArtifactId> {
        self.commit
    }

    pub fn change_set(&self) -> Option<ArtifactId> {
        self.change_set
    }

    pub fn symbols(&self) -> &[ArtifactId] {
        &self.symbols
    }

    pub fn recorded_at(&self) -> &RecordedAt {
        &self.recorded_at
    }
}

impl PatchLineage {
    pub fn patch(&self) -> ArtifactId {
        self.patch
    }

    pub fn agents(&self) -> &[AgentId] {
        &self.agents
    }

    pub fn evidence(&self) -> &[EvidenceId] {
        &self.evidence
    }

    pub fn goals(&self) -> &[GoalId] {
        &self.goals
    }

    pub fn workspaces(&self) -> &[WorkspaceViewId] {
        &self.workspaces
    }

    pub fn verifications(&self) -> &[ArtifactId] {
        &self.verifications
    }

    pub fn commits(&self) -> &[ArtifactId] {
        &self.commits
    }

    pub fn change_sets(&self) -> &[ArtifactId] {
        &self.change_sets
    }

    pub fn symbols(&self) -> &[ArtifactId] {
        &self.symbols
    }
}

impl ProvenanceStore {
    pub fn new() -> Self {
        Self::with_limit(MAX_PROVENANCE_EDGES)
    }

    pub fn with_limit(max_edges: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                edges: Vec::new(),
                by_from: BTreeMap::new(),
                by_to: BTreeMap::new(),
            }),
            max_edges,
        }
    }

    pub fn len(&self) -> Result<usize, ProvenanceError> {
        Ok(self.lock()?.edges.len())
    }

    pub fn is_empty(&self) -> Result<bool, ProvenanceError> {
        Ok(self.lock()?.edges.is_empty())
    }

    /// Append one validated edge. Existing edges are never rewritten.
    pub fn append(
        &self,
        edge: ProvenanceEdge,
        cancel: &CancellationToken,
    ) -> Result<usize, ProvenanceError> {
        cancel.check()?;
        let mut inner = self.lock()?;
        cancel.check()?;
        self.push_locked(&mut inner, edge)
    }

    /// Append a bounded batch atomically. On bound/cancel failure nothing is
    /// written from this batch.
    pub fn append_all(
        &self,
        edges: Vec<ProvenanceEdge>,
        cancel: &CancellationToken,
    ) -> Result<usize, ProvenanceError> {
        cancel.check()?;
        if edges.len() > self.max_edges {
            return Err(ProvenanceError::EdgeLimit {
                limit: self.max_edges,
            });
        }
        for (i, _) in edges.iter().enumerate() {
            if i % CANCEL_STRIDE == 0 {
                cancel.check()?;
            }
        }
        let mut inner = self.lock()?;
        cancel.check()?;
        if inner.edges.len().saturating_add(edges.len()) > self.max_edges {
            return Err(ProvenanceError::EdgeLimit {
                limit: self.max_edges,
            });
        }
        let first = inner.edges.len();
        for (i, edge) in edges.into_iter().enumerate() {
            if i % CANCEL_STRIDE == 0 {
                cancel.check()?;
            }
            let idx = inner.edges.len();
            inner.by_from.entry(edge.from).or_default().push(idx);
            inner.by_to.entry(edge.to).or_default().push(idx);
            inner.edges.push(edge);
        }
        Ok(first)
    }

    /// Record goal/evidence/agent/workspace/patch/verification/commit edges
    /// for one applied patch. All edges share the patch content hash.
    pub fn record_patch_attribution(
        &self,
        attribution: PatchAttribution,
        cancel: &CancellationToken,
    ) -> Result<usize, ProvenanceError> {
        cancel.check()?;
        if attribution.evidence.len() > MAX_EVIDENCE_REFS {
            return Err(ProvenanceError::BoundExceeded);
        }
        let edges = attribution_edges(&attribution)?;
        self.append_all(edges, cancel)
    }

    pub fn edges(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<ProvenanceEdge>, ProvenanceError> {
        cancel.check()?;
        let inner = self.lock()?;
        cancel.check()?;
        Ok(inner.edges.clone())
    }

    pub fn edges_from(
        &self,
        node: ProvenanceNode,
        cancel: &CancellationToken,
    ) -> Result<Vec<ProvenanceEdge>, ProvenanceError> {
        self.indexed_edges(node, true, cancel)
    }

    pub fn edges_to(
        &self,
        node: ProvenanceNode,
        cancel: &CancellationToken,
    ) -> Result<Vec<ProvenanceEdge>, ProvenanceError> {
        self.indexed_edges(node, false, cancel)
    }

    /// Walk patch (and derived change-set) edges back to agent and evidence.
    pub fn lineage_for_patch(
        &self,
        patch: ArtifactId,
        cancel: &CancellationToken,
    ) -> Result<PatchLineage, ProvenanceError> {
        cancel.check()?;
        let inner = self.lock()?;
        cancel.check()?;
        let patch_node = ProvenanceNode::Patch(patch);
        let outgoing = match inner.by_from.get(&patch_node) {
            Some(idxs) if !idxs.is_empty() => idxs,
            _ => return Err(ProvenanceError::PatchNotFound),
        };

        let mut lineage = PatchLineage {
            patch,
            agents: Vec::new(),
            evidence: Vec::new(),
            goals: Vec::new(),
            workspaces: Vec::new(),
            verifications: Vec::new(),
            commits: Vec::new(),
            change_sets: Vec::new(),
            symbols: Vec::new(),
        };

        collect_from_indices(&inner, outgoing, &mut lineage, cancel)?;

        let inbound = inner.by_to.get(&patch_node).cloned().unwrap_or_default();
        for (i, idx) in inbound.iter().copied().enumerate() {
            if i % CANCEL_STRIDE == 0 {
                cancel.check()?;
            }
            let Some(edge) = inner.edges.get(idx) else {
                return Err(ProvenanceError::LockPoisoned);
            };
            if edge.kind != ProvenanceEdgeKind::DerivedFrom {
                continue;
            }
            if let Some(change_set) = edge.from.as_change_set() {
                push_unique(&mut lineage.change_sets, change_set);
                if let Some(from_idxs) = inner.by_from.get(&edge.from) {
                    collect_from_indices(&inner, from_idxs, &mut lineage, cancel)?;
                }
            }
        }

        Ok(lineage)
    }

    fn indexed_edges(
        &self,
        node: ProvenanceNode,
        outgoing: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<ProvenanceEdge>, ProvenanceError> {
        cancel.check()?;
        let inner = self.lock()?;
        cancel.check()?;
        let idxs = if outgoing {
            inner.by_from.get(&node)
        } else {
            inner.by_to.get(&node)
        };
        let Some(idxs) = idxs else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(idxs.len());
        for (i, idx) in idxs.iter().copied().enumerate() {
            if i % CANCEL_STRIDE == 0 {
                cancel.check()?;
            }
            let Some(edge) = inner.edges.get(idx) else {
                return Err(ProvenanceError::LockPoisoned);
            };
            out.push(edge.clone());
        }
        Ok(out)
    }

    fn push_locked(
        &self,
        inner: &mut Inner,
        edge: ProvenanceEdge,
    ) -> Result<usize, ProvenanceError> {
        if inner.edges.len() >= self.max_edges {
            return Err(ProvenanceError::EdgeLimit {
                limit: self.max_edges,
            });
        }
        let idx = inner.edges.len();
        inner.by_from.entry(edge.from).or_default().push(idx);
        inner.by_to.entry(edge.to).or_default().push(idx);
        inner.edges.push(edge);
        Ok(idx)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, ProvenanceError> {
        self.inner.lock().map_err(|_| ProvenanceError::LockPoisoned)
    }
}

impl Default for ProvenanceStore {
    fn default() -> Self {
        Self::new()
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

    pub fn check(&self) -> Result<(), ProvenanceError> {
        if self.is_cancelled() {
            Err(ProvenanceError::Cancelled)
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

impl fmt::Display for ProvenanceEdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for RecordedAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rfc3339)
    }
}

impl fmt::Display for ProvenanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("provenance operation cancelled"),
            Self::BoundExceeded => f.write_str("provenance resource bound exceeded"),
            Self::EdgeLimit { .. } => f.write_str("provenance edge limit reached"),
            Self::InvalidEdge => f.write_str("provenance edge endpoints do not match kind"),
            Self::InvalidTimestamp => f.write_str("provenance timestamp is invalid"),
            Self::PatchNotFound => f.write_str("provenance patch has no recorded edges"),
            Self::LockPoisoned => f.write_str("provenance store lock poisoned"),
            Self::UnknownVariant => f.write_str("unknown provenance enumeration value"),
            Self::UnsupportedSchema => f.write_str("unsupported provenance edge schema"),
            Self::UnsupportedSchemaVersion => {
                f.write_str("unsupported provenance edge schema version")
            }
        }
    }
}

impl Error for ProvenanceError {}

impl FromStr for ProvenanceEdgeKind {
    type Err = ProvenanceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for item in Self::ALL {
            if item.as_str() == s {
                return Ok(*item);
            }
        }
        Err(ProvenanceError::UnknownVariant)
    }
}

impl FromStr for RecordedAt {
    type Err = ProvenanceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_recorded_at(s)
    }
}

impl Serialize for ProvenanceEdgeKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProvenanceEdgeKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

impl Serialize for RecordedAt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.rfc3339)
    }
}

impl<'de> Deserialize<'de> for RecordedAt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

impl Serialize for ProvenanceNode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ProvenanceNode", 2)?;
        state.serialize_field("kind", self.kind_str())?;
        state.serialize_field("id", &self.id_string())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProvenanceNode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawNode::deserialize(deserializer)?;
        Self::parse(&raw.kind, &raw.id).map_err(de::Error::custom)
    }
}

impl Serialize for ProvenanceEdge {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ProvenanceEdge", EDGE_FIELDS.len())?;
        state.serialize_field("schema", PROVENANCE_EDGE_SCHEMA)?;
        state.serialize_field("schema_version", &PROVENANCE_EDGE_SCHEMA_VERSION)?;
        state.serialize_field("from", &self.from)?;
        state.serialize_field("to", &self.to)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("recorded_at", &self.recorded_at)?;
        state.serialize_field("content_hash", &self.content_hash)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProvenanceEdge {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawEdge::deserialize(deserializer)?;
        if raw.schema != PROVENANCE_EDGE_SCHEMA {
            return Err(de::Error::custom(ProvenanceError::UnsupportedSchema));
        }
        if raw.schema_version != PROVENANCE_EDGE_SCHEMA_VERSION {
            return Err(de::Error::custom(ProvenanceError::UnsupportedSchemaVersion));
        }
        ProvenanceEdge::new(
            raw.from,
            raw.to,
            raw.kind,
            raw.recorded_at,
            raw.content_hash,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNode {
    kind: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEdge {
    schema: String,
    schema_version: u16,
    from: ProvenanceNode,
    to: ProvenanceNode,
    kind: ProvenanceEdgeKind,
    recorded_at: RecordedAt,
    content_hash: ArtifactId,
}

fn attribution_edges(
    attribution: &PatchAttribution,
) -> Result<Vec<ProvenanceEdge>, ProvenanceError> {
    let patch = ProvenanceNode::Patch(attribution.patch);
    let hash = attribution.patch;
    let at = attribution.recorded_at.clone();
    let mut edges = Vec::new();
    for symbol in &attribution.symbols {
        edges.push(ProvenanceEdge::new(
            patch,
            ProvenanceNode::Symbol(*symbol),
            ProvenanceEdgeKind::ModifiesSymbol,
            at.clone(),
            hash,
        )?);
    }
    edges.push(ProvenanceEdge::new(
        patch,
        ProvenanceNode::Agent(attribution.agent),
        ProvenanceEdgeKind::ProducedBy,
        at.clone(),
        hash,
    )?);
    for evidence in &attribution.evidence {
        edges.push(ProvenanceEdge::new(
            patch,
            ProvenanceNode::Evidence(*evidence),
            ProvenanceEdgeKind::SupportedBy,
            at.clone(),
            hash,
        )?);
    }
    if let Some(goal) = attribution.goal {
        edges.push(ProvenanceEdge::new(
            patch,
            ProvenanceNode::Goal(goal),
            ProvenanceEdgeKind::AttributedTo,
            at.clone(),
            hash,
        )?);
    }
    if let Some(workspace) = attribution.workspace {
        edges.push(ProvenanceEdge::new(
            patch,
            ProvenanceNode::Workspace(workspace),
            ProvenanceEdgeKind::AppliedTo,
            at.clone(),
            hash,
        )?);
    }
    if let Some(verification) = attribution.verification {
        edges.push(ProvenanceEdge::new(
            patch,
            ProvenanceNode::Verification(verification),
            ProvenanceEdgeKind::VerifiedBy,
            at.clone(),
            hash,
        )?);
    }
    if let Some(commit) = attribution.commit {
        edges.push(ProvenanceEdge::new(
            patch,
            ProvenanceNode::Commit(commit),
            ProvenanceEdgeKind::CommittedAs,
            at.clone(),
            hash,
        )?);
    }
    if let Some(change_set) = attribution.change_set {
        let cs = ProvenanceNode::ChangeSet(change_set);
        edges.push(ProvenanceEdge::new(
            cs,
            patch,
            ProvenanceEdgeKind::DerivedFrom,
            at.clone(),
            hash,
        )?);
        edges.push(ProvenanceEdge::new(
            cs,
            ProvenanceNode::Agent(attribution.agent),
            ProvenanceEdgeKind::ProducedBy,
            at.clone(),
            hash,
        )?);
        for evidence in &attribution.evidence {
            edges.push(ProvenanceEdge::new(
                cs,
                ProvenanceNode::Evidence(*evidence),
                ProvenanceEdgeKind::SupportedBy,
                at.clone(),
                hash,
            )?);
        }
    }
    Ok(edges)
}

fn collect_from_indices(
    inner: &Inner,
    idxs: &[usize],
    lineage: &mut PatchLineage,
    cancel: &CancellationToken,
) -> Result<(), ProvenanceError> {
    for (i, idx) in idxs.iter().copied().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            cancel.check()?;
        }
        let Some(edge) = inner.edges.get(idx) else {
            return Err(ProvenanceError::LockPoisoned);
        };
        match edge.kind {
            ProvenanceEdgeKind::ProducedBy => {
                if let Some(agent) = edge.to.as_agent() {
                    push_unique(&mut lineage.agents, agent);
                }
            }
            ProvenanceEdgeKind::SupportedBy => {
                if let Some(evidence) = edge.to.as_evidence() {
                    push_unique(&mut lineage.evidence, evidence);
                }
            }
            ProvenanceEdgeKind::AttributedTo => {
                if let Some(goal) = edge.to.as_goal() {
                    push_unique(&mut lineage.goals, goal);
                }
            }
            ProvenanceEdgeKind::AppliedTo => {
                if let Some(workspace) = edge.to.as_workspace() {
                    push_unique(&mut lineage.workspaces, workspace);
                }
            }
            ProvenanceEdgeKind::VerifiedBy => {
                if let Some(verification) = edge.to.as_verification() {
                    push_unique(&mut lineage.verifications, verification);
                }
            }
            ProvenanceEdgeKind::CommittedAs => {
                if let Some(commit) = edge.to.as_commit() {
                    push_unique(&mut lineage.commits, commit);
                }
            }
            ProvenanceEdgeKind::DerivedFrom => {
                if let Some(patch) = edge.to.as_patch()
                    && patch != lineage.patch
                {
                    return Err(ProvenanceError::InvalidEdge);
                }
            }
            ProvenanceEdgeKind::ModifiesSymbol => {
                if let Some(symbol) = edge.to.as_symbol() {
                    push_unique(&mut lineage.symbols, symbol);
                }
            }
        }
    }
    Ok(())
}

fn push_unique<T: Copy + Eq>(out: &mut Vec<T>, value: T) {
    if out.len() >= MAX_LINEAGE_NODES {
        return;
    }
    if !out.contains(&value) {
        out.push(value);
    }
}

fn validate_edge(
    from: ProvenanceNode,
    to: ProvenanceNode,
    kind: ProvenanceEdgeKind,
) -> Result<(), ProvenanceError> {
    if from == to {
        return Err(ProvenanceError::InvalidEdge);
    }
    let ok = match kind {
        ProvenanceEdgeKind::ProducedBy => {
            matches!(to, ProvenanceNode::Agent(_))
                && matches!(
                    from,
                    ProvenanceNode::Patch(_)
                        | ProvenanceNode::ChangeSet(_)
                        | ProvenanceNode::Task(_)
                        | ProvenanceNode::ModelStep(_)
                )
        }
        ProvenanceEdgeKind::AttributedTo => {
            matches!(to, ProvenanceNode::Goal(_))
                && matches!(
                    from,
                    ProvenanceNode::Patch(_)
                        | ProvenanceNode::ChangeSet(_)
                        | ProvenanceNode::Task(_)
                        | ProvenanceNode::DecisionRecord(_)
                        | ProvenanceNode::Verification(_)
                )
        }
        ProvenanceEdgeKind::SupportedBy => {
            matches!(to, ProvenanceNode::Evidence(_))
                && matches!(
                    from,
                    ProvenanceNode::Patch(_)
                        | ProvenanceNode::ChangeSet(_)
                        | ProvenanceNode::Verification(_)
                        | ProvenanceNode::Commit(_)
                )
        }
        ProvenanceEdgeKind::AppliedTo => {
            matches!(to, ProvenanceNode::Workspace(_))
                && matches!(
                    from,
                    ProvenanceNode::Patch(_) | ProvenanceNode::ChangeSet(_)
                )
        }
        ProvenanceEdgeKind::VerifiedBy => {
            matches!(to, ProvenanceNode::Verification(_))
                && matches!(
                    from,
                    ProvenanceNode::Patch(_)
                        | ProvenanceNode::ChangeSet(_)
                        | ProvenanceNode::Commit(_)
                )
        }
        ProvenanceEdgeKind::CommittedAs => {
            matches!(to, ProvenanceNode::Commit(_))
                && matches!(
                    from,
                    ProvenanceNode::Patch(_) | ProvenanceNode::ChangeSet(_)
                )
        }
        ProvenanceEdgeKind::DerivedFrom => {
            matches!(from, ProvenanceNode::ChangeSet(_)) && matches!(to, ProvenanceNode::Patch(_))
        }
        ProvenanceEdgeKind::ModifiesSymbol => {
            matches!(to, ProvenanceNode::Symbol(_))
                && matches!(
                    from,
                    ProvenanceNode::Patch(_) | ProvenanceNode::ChangeSet(_)
                )
        }
    };
    if ok {
        Ok(())
    } else {
        Err(ProvenanceError::InvalidEdge)
    }
}

fn parse_id<T: FromStr>(s: &str) -> Result<T, ProvenanceError> {
    s.parse().map_err(|_| ProvenanceError::UnknownVariant)
}

fn parse_hash(s: &str) -> Result<ArtifactId, ProvenanceError> {
    s.parse().map_err(|_| ProvenanceError::UnknownVariant)
}

fn parse_recorded_at(s: &str) -> Result<RecordedAt, ProvenanceError> {
    if s.len() < 20 || s.len() > 40 {
        return Err(ProvenanceError::InvalidTimestamp);
    }
    let prefix = s.get(..19).ok_or(ProvenanceError::InvalidTimestamp)?;
    if !is_valid_datetime_prefix(prefix) {
        return Err(ProvenanceError::InvalidTimestamp);
    }
    let rest = &s[19..];
    if rest == "Z" {
        return Ok(RecordedAt {
            rfc3339: s.to_owned(),
        });
    }
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac
            .strip_suffix('Z')
            .ok_or(ProvenanceError::InvalidTimestamp)?;
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ProvenanceError::InvalidTimestamp);
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
    Err(ProvenanceError::InvalidTimestamp)
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
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn unix_secs_to_rfc3339(secs: u64) -> Result<String, ProvenanceError> {
    const SECS_PER_DAY: u64 = 86_400;
    let days = i64::try_from(secs / SECS_PER_DAY).map_err(|_| ProvenanceError::InvalidTimestamp)?;
    let rem = secs % SECS_PER_DAY;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return Err(ProvenanceError::InvalidTimestamp);
    }
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z"
    ))
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_AT: &str = "2026-08-15T12:00:00Z";
    const AGENT: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const EVIDENCE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
    const GOAL: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const VIEW: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";

    fn at() -> RecordedAt {
        GOLDEN_AT.parse().expect("golden timestamp")
    }

    fn agent() -> AgentId {
        AGENT.parse().expect("agent")
    }

    fn evidence() -> EvidenceId {
        EVIDENCE.parse().expect("evidence")
    }

    fn goal() -> GoalId {
        GOAL.parse().expect("goal")
    }

    fn view() -> WorkspaceViewId {
        VIEW.parse().expect("view")
    }

    fn patch_hash() -> ArtifactId {
        ArtifactId::from_bytes(b"semantic-patch-v1")
    }

    fn commit_hash() -> ArtifactId {
        ArtifactId::from_bytes(b"commit-receipt-v1")
    }

    fn verification_hash() -> ArtifactId {
        ArtifactId::from_bytes(b"verification-report-v1")
    }

    fn change_set_hash() -> ArtifactId {
        ArtifactId::from_bytes(b"change-set-v1")
    }

    fn symbol_hash() -> ArtifactId {
        ArtifactId::from_bytes(b"src/lib.rs::parse_base_revision")
    }

    fn recorded_patch() -> PatchAttribution {
        PatchAttribution::new(patch_hash(), agent(), at())
            .with_evidence([evidence()])
            .with_goal(goal())
            .with_workspace(view())
            .with_verification(verification_hash())
            .with_commit(commit_hash())
            .with_change_set(change_set_hash())
            .with_symbols([symbol_hash()])
    }

    #[test]
    fn applied_patch_traces_to_agent_and_evidence() {
        let store = ProvenanceStore::new();
        let cancel = CancellationToken::new();
        store
            .record_patch_attribution(recorded_patch(), &cancel)
            .expect("record");

        let lineage = store
            .lineage_for_patch(patch_hash(), &cancel)
            .expect("lineage");
        assert_eq!(lineage.patch(), patch_hash());
        assert_eq!(lineage.agents(), &[agent()]);
        assert_eq!(lineage.evidence(), &[evidence()]);
        assert_eq!(lineage.goals(), &[goal()]);
        assert_eq!(lineage.workspaces(), &[view()]);
        assert_eq!(lineage.verifications(), &[verification_hash()]);
        assert_eq!(lineage.commits(), &[commit_hash()]);
        assert_eq!(lineage.change_sets(), &[change_set_hash()]);
        assert_eq!(lineage.symbols(), &[symbol_hash()]);
    }

    #[test]
    fn edges_reference_hashes_and_ids_not_display_text() {
        let edge = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Agent(agent()),
            ProvenanceEdgeKind::ProducedBy,
            at(),
            patch_hash(),
        )
        .expect("edge");
        let json = serde_json::to_string(&edge).expect("serialize");
        assert!(json.contains(AGENT));
        assert!(json.contains(&patch_hash().to_string()));
        assert!(!json.contains("statement"));
        assert!(!json.contains("title"));
        assert!(!json.contains("rationale"));
        assert!(!json.contains("display"));
        let decoded: ProvenanceEdge = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, edge);
        assert_eq!(decoded.content_hash(), patch_hash());
    }

    #[test]
    fn store_is_append_only() {
        let store = ProvenanceStore::new();
        let cancel = CancellationToken::new();
        let first = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Agent(agent()),
            ProvenanceEdgeKind::ProducedBy,
            at(),
            patch_hash(),
        )
        .expect("first");
        store.append(first.clone(), &cancel).expect("append first");
        let later = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Evidence(evidence()),
            ProvenanceEdgeKind::SupportedBy,
            at(),
            patch_hash(),
        )
        .expect("later");
        store.append(later, &cancel).expect("append later");
        let edges = store.edges(&cancel).expect("edges");
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0], first);
        assert_eq!(store.len().expect("len"), 2);
    }

    #[test]
    fn rejects_invalid_kind_and_self_loop() {
        let err = ProvenanceEdge::new(
            ProvenanceNode::Agent(agent()),
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceEdgeKind::ProducedBy,
            at(),
            patch_hash(),
        )
        .expect_err("reversed produced_by");
        assert_eq!(err, ProvenanceError::InvalidEdge);

        let loop_err = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceEdgeKind::DerivedFrom,
            at(),
            patch_hash(),
        )
        .expect_err("self loop");
        assert_eq!(loop_err, ProvenanceError::InvalidEdge);
    }

    #[test]
    fn cancelled_record_writes_nothing() {
        let store = ProvenanceStore::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = store
            .record_patch_attribution(recorded_patch(), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, ProvenanceError::Cancelled);
        assert!(store.is_empty().expect("empty"));
    }

    #[test]
    fn bound_exceeded_is_atomic() {
        let store = ProvenanceStore::with_limit(1);
        let cancel = CancellationToken::new();
        let err = store
            .record_patch_attribution(recorded_patch(), &cancel)
            .expect_err("over limit");
        assert_eq!(err, ProvenanceError::EdgeLimit { limit: 1 });
        assert!(store.is_empty().expect("empty"));
    }

    #[test]
    fn missing_patch_lineage_is_typed() {
        let store = ProvenanceStore::new();
        let cancel = CancellationToken::new();
        let err = store
            .lineage_for_patch(patch_hash(), &cancel)
            .expect_err("missing");
        assert_eq!(err, ProvenanceError::PatchNotFound);
    }

    #[test]
    fn evidence_ref_bound() {
        let store = ProvenanceStore::new();
        let cancel = CancellationToken::new();
        let extra: Vec<EvidenceId> = (0..=MAX_EVIDENCE_REFS).map(|_| EvidenceId::new()).collect();
        let err = store
            .record_patch_attribution(
                PatchAttribution::new(patch_hash(), agent(), at()).with_evidence(extra),
                &cancel,
            )
            .expect_err("too many evidence refs");
        assert_eq!(err, ProvenanceError::BoundExceeded);
        assert!(store.is_empty().expect("empty"));
    }

    #[test]
    fn golden_edge_round_trips() {
        let edge = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Agent(agent()),
            ProvenanceEdgeKind::ProducedBy,
            at(),
            patch_hash(),
        )
        .expect("edge");
        let json = serde_json::to_value(&edge).expect("json");
        assert_eq!(json["schema"], PROVENANCE_EDGE_SCHEMA);
        assert_eq!(json["schema_version"], PROVENANCE_EDGE_SCHEMA_VERSION);
        assert_eq!(json["kind"], "produced_by");
        assert_eq!(json["from"]["kind"], "patch");
        assert_eq!(json["to"]["kind"], "agent");
        assert_eq!(json["to"]["id"], AGENT);
        assert_eq!(json["recorded_at"], GOLDEN_AT);
        assert_eq!(json["content_hash"], patch_hash().to_string());
        let decoded: ProvenanceEdge = serde_json::from_value(json).expect("decode");
        assert_eq!(decoded, edge);
    }

    #[test]
    fn closed_schema_rejects_unknown_fields_and_versions() {
        let hash = patch_hash().to_string();
        let bad_field = format!(
            r#"{{"schema":"{PROVENANCE_EDGE_SCHEMA}","schema_version":1,"from":{{"kind":"patch","id":"{hash}"}},"to":{{"kind":"agent","id":"{AGENT}"}},"kind":"produced_by","recorded_at":"{GOLDEN_AT}","content_hash":"{hash}","label":"nope"}}"#
        );
        assert!(serde_json::from_str::<ProvenanceEdge>(&bad_field).is_err());

        let bad_schema = format!(
            r#"{{"schema":"other","schema_version":1,"from":{{"kind":"patch","id":"{hash}"}},"to":{{"kind":"agent","id":"{AGENT}"}},"kind":"produced_by","recorded_at":"{GOLDEN_AT}","content_hash":"{hash}"}}"#
        );
        assert!(serde_json::from_str::<ProvenanceEdge>(&bad_schema).is_err());

        let node = r#"{"kind":"agent","id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","name":"x"}"#;
        assert!(serde_json::from_str::<ProvenanceNode>(node).is_err());
    }

    #[test]
    fn recorded_at_unix_epoch_formats_canonically() {
        let stamp = RecordedAt::from_unix_secs(0).expect("epoch");
        assert_eq!(stamp.as_str(), "1970-01-01T00:00:00Z");
        let leap = RecordedAt::from_unix_secs(1_582_934_400).expect("leap day");
        assert_eq!(leap.as_str(), "2020-02-29T00:00:00Z");
    }

    #[test]
    fn task_and_model_step_edges_are_id_backed() {
        let task: TaskId = "018f3c8a-7e2b-7a10-8c4d-0123456789af".parse().unwrap();
        let step: ModelStepId = "018f3c8a-7e2b-7a10-8c4d-0123456789b0".parse().unwrap();
        let store = ProvenanceStore::new();
        let cancel = CancellationToken::new();
        store
            .append(
                ProvenanceEdge::new(
                    ProvenanceNode::Task(task),
                    ProvenanceNode::Agent(agent()),
                    ProvenanceEdgeKind::ProducedBy,
                    at(),
                    patch_hash(),
                )
                .unwrap(),
                &cancel,
            )
            .unwrap();
        store
            .append(
                ProvenanceEdge::new(
                    ProvenanceNode::ModelStep(step),
                    ProvenanceNode::Agent(agent()),
                    ProvenanceEdgeKind::ProducedBy,
                    at(),
                    patch_hash(),
                )
                .unwrap(),
                &cancel,
            )
            .unwrap();
        let outgoing = store
            .edges_to(ProvenanceNode::Agent(agent()), &cancel)
            .unwrap();
        assert_eq!(outgoing.len(), 2);
        let json = serde_json::to_string(&outgoing[0]).unwrap();
        assert!(!json.contains("prompt"));
        assert!(!json.contains("hidden"));
    }

    #[test]
    fn symbol_edge_wire_round_trips_and_hides_display_text() {
        let edge = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Symbol(symbol_hash()),
            ProvenanceEdgeKind::ModifiesSymbol,
            at(),
            patch_hash(),
        )
        .expect("symbol edge");
        let value = serde_json::to_value(&edge).expect("serialize");
        let json = value.to_string();
        assert!(json.contains(&symbol_hash().to_string()));
        assert_eq!(value["from"]["kind"], "patch");
        assert_eq!(value["to"]["kind"], "symbol");
        assert_eq!(value["kind"], "modifies_symbol");
        // The mutable symbol name is never stored; only its content hash is.
        assert!(!json.contains("parse_base_revision"));
        assert!(!json.contains("src/lib.rs"));
        let decoded: ProvenanceEdge = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, edge);
        assert_eq!(decoded.kind(), ProvenanceEdgeKind::ModifiesSymbol);
    }

    #[test]
    fn patch_attributes_to_symbols_and_lineage_reports_them() {
        let store = ProvenanceStore::new();
        let cancel = CancellationToken::new();
        let extra = ArtifactId::from_bytes(b"src/parse.rs::tokenize");
        store
            .record_patch_attribution(recorded_patch().with_symbols([extra]), &cancel)
            .expect("record");

        let lineage = store
            .lineage_for_patch(patch_hash(), &cancel)
            .expect("lineage");
        assert_eq!(lineage.symbols(), &[symbol_hash(), extra]);

        let edges = store
            .edges_from(ProvenanceNode::Patch(patch_hash()), &cancel)
            .expect("edges");
        let symbol_edges: Vec<_> = edges
            .iter()
            .filter(|edge| edge.kind() == ProvenanceEdgeKind::ModifiesSymbol)
            .collect();
        assert_eq!(symbol_edges.len(), 2);
        for edge in &symbol_edges {
            assert!(matches!(edge.to(), ProvenanceNode::Symbol(_)));
        }
        let mut symbols: Vec<_> = symbol_edges
            .iter()
            .map(|edge| edge.to().as_symbol().expect("symbol"))
            .collect();
        symbols.sort();
        assert_eq!(symbols.len(), 2);
    }

    #[test]
    fn modifies_symbol_rejects_wrong_endpoints() {
        let err = ProvenanceEdge::new(
            ProvenanceNode::Symbol(symbol_hash()),
            ProvenanceNode::Agent(agent()),
            ProvenanceEdgeKind::ModifiesSymbol,
            at(),
            patch_hash(),
        )
        .expect_err("symbol as source");
        assert_eq!(err, ProvenanceError::InvalidEdge);

        let err = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Agent(agent()),
            ProvenanceEdgeKind::ModifiesSymbol,
            at(),
            patch_hash(),
        )
        .expect_err("agent as target");
        assert_eq!(err, ProvenanceError::InvalidEdge);

        // A symbol cannot be produced by an agent in a patch attribution.
        let err = ProvenanceEdge::new(
            ProvenanceNode::Patch(patch_hash()),
            ProvenanceNode::Symbol(symbol_hash()),
            ProvenanceEdgeKind::ProducedBy,
            at(),
            patch_hash(),
        )
        .expect_err("produced_by to symbol");
        assert_eq!(err, ProvenanceError::InvalidEdge);
    }

    #[test]
    fn symbol_node_round_trips_without_display_text() {
        let node = ProvenanceNode::Symbol(symbol_hash());
        let value = serde_json::to_value(&node).expect("serialize");
        assert_eq!(value["kind"], "symbol");
        assert_eq!(value["id"], symbol_hash().to_string());
        let decoded: ProvenanceNode = serde_json::from_str(&value.to_string()).expect("decode");
        assert_eq!(decoded, node);
        // Unknown symbol identity is rejected fail-closed.
        let bad = r#"{"kind":"symbol","id":"not-a-hash"}"#;
        assert!(serde_json::from_str::<ProvenanceNode>(bad).is_err());
    }
}
