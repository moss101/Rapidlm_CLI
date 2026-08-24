# Canonical Domain Model

This document defines the names and invariants that all Rust crates, the TypeScript SDK, JSONL events, ACP adapters, and persistence schemas share. Public wire representations use `snake_case` JSON and UUIDv7 string identifiers unless explicitly stated.

## Aggregate ownership

| Aggregate | Owner | Durable? | Key invariants |
|---|---|---:|---|
| `Session` | Kernel/Event Ledger | yes | ordered event stream; one active turn; at most one top-level goal snapshot |
| `Agent` | Agent Runtime | yes | immutable parent/scope/view association; terminal states immutable |
| `Goal` | Goal Runtime | yes | `active|paused|blocked`; completion/cancel terminal events; required evidence gates completion |
| `WorkspaceView` | Workspace | yes metadata | one write owner at a time; base revision immutable; mutations journaled |
| `CapabilityLease` | Capability Broker | audit yes | action hash, principal, scope, expiry and remaining uses must match execution |
| `Job` | Process Supervisor | yes | parentage, cancellation policy, output artifact refs, final exit status |
| `Evidence` | Goal/Evidence service | yes | typed observation, producer, source hash, verification status |
| `Artifact` | Artifact Store | yes | content-addressed immutable blob; metadata can be redacted/expired |
| `ContextItem` | Context Engine | cache + memory | provenance, freshness, token estimate and reason required |
| `ModelCall` | LLM Router | events/metrics | provider/model/policy version, usage, latency, outcome |

## Common primitives

```rust
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Id<T>(pub uuid::Uuid, #[serde(skip)] PhantomData<T>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,               // sha256:<64 lowercase hex>
    pub media_type: String,
    pub bytes: u64,
    pub redaction: RedactionClass,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RedactionClass { Public, Project, Sensitive, Secret }
```

## Session

```rust
pub struct SessionSnapshot {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub status: SessionStatus,
    pub active_turn: Option<TurnId>,
    pub top_level_goal: Option<GoalSnapshot>,
    pub active_agents: Vec<AgentId>,
    pub seq: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub enum SessionStatus { Ready, Busy, Paused, Recovering, Closed }
```

A session fork starts a new event stream and workspace view. It may copy conversation projection and pinned context, but MUST NOT inherit an `active` top-level goal or capability leases.

## Agent

```rust
pub struct AgentSpec {
    pub id: AgentId,
    pub parent_id: Option<AgentId>,
    pub role: AgentRole,
    pub task: String,
    pub workspace_view_id: WorkspaceViewId,
    pub model_policy: ModelPolicyRef,
    pub budget: AgentBudget,
    pub permissions_profile: String,
}

pub enum AgentState { Queued, Starting, Running, WaitingTool, WaitingApproval, Paused, Blocked, Succeeded, Failed, Cancelled }
```

Subagents return `AgentResult`; they do not directly mutate the top-level goal lifecycle.

## Goal and evidence

```rust
pub enum GoalState { Active, Paused, Blocked }

pub struct GoalSnapshot {
    pub id: GoalId,
    pub statement: String,
    pub completion_criteria: Vec<Criterion>,
    pub state: GoalState,
    pub stop_reason: Option<GoalStopReason>,
    pub budget: GoalBudget,
    pub usage: GoalUsage,
    pub evidence_requirements: Vec<EvidenceRequirement>,
}

pub struct EvidenceRecord {
    pub id: EvidenceId,
    pub goal_id: GoalId,
    pub criterion_id: Option<String>,
    pub kind: EvidenceKind,
    pub assertion: String,
    pub producer: ActorRef,
    pub source: EvidenceSource,
    pub observed_at: DateTime<Utc>,
    pub status: EvidenceStatus,
}
```

Evidence kinds include `test`, `build`, `lint`, `scan`, `diff`, `runtime_observation`, `user_confirmation`, `external_attestation`, and `manual_review`. Required evidence is validated by runtime logic, never by prompt text alone.

## Workspace and semantic patch

```rust
pub struct WorkspaceView {
    pub id: WorkspaceViewId,
    pub repo_id: RepoId,
    pub backend: WorkspaceBackend,
    pub base_revision: String,
    pub write_owner: Option<AgentId>,
    pub state: WorkspaceState,
}

pub enum PatchOp {
    ReplaceRange { path: RepoPath, preimage_sha256: String, start: BytePos, end: BytePos, content: String },
    CreateFile { path: RepoPath, content: Vec<u8>, executable: bool },
    DeleteFile { path: RepoPath, preimage_sha256: String },
    MoveFile { from: RepoPath, to: RepoPath, preimage_sha256: String },
}
```

A transaction commits only if all preimage checks pass and no path violates policy. Shell-induced mutations are represented as `ExternalMutation` until reconciled.

## Capability lease

```rust
pub struct CapabilityLease {
    pub lease_id: LeaseId,
    pub principal: PrincipalRef,
    pub action_hash: [u8; 32],
    pub capability: Capability,
    pub resource_scope: ResourceScope,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub max_uses: u32,
    pub policy_revision: String,
}
```

Executors validate the lease again immediately before the privileged side effect. Leases are not transferable across agents, sessions, views, commands, URL origins, or resolved filesystem scopes unless explicitly encoded.

## Context item

```rust
pub struct ContextItem {
    pub id: ContextItemId,
    pub source: ContextSource,
    pub repo_id: Option<RepoId>,
    pub locator: String,
    pub content_hash: String,
    pub text: String,
    pub estimated_tokens: u32,
    pub freshness: Freshness,
    pub trust: TrustClass,
    pub reason: InclusionReason,
    pub score: f32,
}
```

Model-visible context MUST preserve untrusted-data labels when content originated from repository files, web pages, MCP servers, terminal output, browser pages, or user-controlled documents.

## Error model

All public errors use a stable machine code plus safe message:

```json
{"code":"policy.denied","message":"Action denied by project policy","retryable":false,"trace_id":"...","details":{}}
```

Internal causes may be logged after redaction but MUST NOT expose credentials or hidden prompts through public error details.

## V2 domain additions — managed agents, handoff, knowledge, control and trajectories

```rust
pub struct AgentMessage {
    pub id: MessageId,
    pub from: AgentId,
    pub to: AgentRecipient,
    pub topic: MailboxTopic,
    pub body: String,
    pub evidence: Vec<EvidenceId>,
    pub artifacts: Vec<ArtifactRef>,
    pub observed_at: DateTime<Utc>,
}

pub struct SessionExecutionLease {
    pub session_id: SessionId,
    pub holder_runtime: RuntimeId,
    pub generation: u64,
    pub expires_at: DateTime<Utc>,
}

pub struct ControlLease {
    pub id: ControlLeaseId,
    pub session_id: SessionId,
    pub holder: ControlActor,
    pub domains: BTreeSet<ControlDomain>,
    pub generation: u64,
    pub expires_at: Option<DateTime<Utc>>,
}

pub struct KnowledgeItem {
    pub id: KnowledgeId,
    pub title: String,
    pub body: String,
    pub scope: KnowledgeScope,
    pub triggers: Vec<KnowledgeTrigger>,
    pub evidence: Vec<EvidenceSourceRef>,
    pub owner: PrincipalRef,
    pub status: KnowledgeStatus,
    pub verify_after: Option<DateTime<Utc>>,
}

pub struct HandoffBundleRef {
    pub handoff_id: HandoffId,
    pub artifact: ArtifactRef,
    pub source_generation: u64,
    pub expires_at: DateTime<Utc>,
}

pub struct TrainingTrajectoryRef {
    pub id: TrajectoryId,
    pub artifact: ArtifactRef,
    pub data_policy: DataPolicyLabel,
    pub outcome: OutcomeLabel,
    pub reward: RewardVector,
}
```

V2 invariants:

- `SessionExecutionLease` has exactly one valid write owner per generation; transfer is compare-and-swap/transactional.
- `ControlLease` controls interactive ownership but never grants security capabilities.
- `KnowledgeItem` is data/instructional context, not a capability or policy grant.
- trajectory records never require hidden chain-of-thought; they reference observable events/artifacts.
- persistent background agents are read-only by default and may communicate only through typed mailboxes/events.
