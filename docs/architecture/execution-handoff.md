# Architecture — Execution Handoff

## 1. Responsibility

Move a durable RapidLM session between execution hosts—local process, daemon, SSH workstation, CI runner, Kubernetes worker, hosted gVisor/microVM worker, GPU worker, or remote macOS worker—without losing goal, evidence, workspace, agent topology or observable session history.

The design is inspired by Devin CLI `/handoff`, generalized into a provider-neutral runtime protocol.

## 2. Non-responsibilities

- Handoff does not migrate an already-running OS process image.
- Capability leases and secret values are never transferred.
- Handoff is not Git push. Git is one payload source; uncommitted transactional changes and runtime state are separate.
- Remote workers do not become trusted merely because they accepted a handoff.

## 3. Core invariant: no split-brain execution

At most one host owns the write-capable `SessionExecutionLease` for a session generation.

```text
SOURCE ACTIVE
   │
   ├─ request handoff
   ▼
SOURCE PARKING ── flush ledger / stop tool dispatch / quiesce writers
   │
   ├─ build signed HandoffBundle
   ▼
TARGET RESTORING ── verify bundle / materialize repo / apply ChangeSet
   │
   ├─ target passes compatibility + policy checks
   ▼
LEASE TRANSFER COMMIT
   │
   ├─ source releases generation N
   └─ target acquires generation N+1
   ▼
TARGET PAUSED/READY → explicit resume or authorized auto-resume
```

If transfer cannot commit, source remains parked with a resumable local session; target destroys provisional state.

## 4. HandoffBundle

```rust
pub struct HandoffBundle {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub source_runtime: RuntimeIdentity,
    pub source_generation: u64,
    pub ledger_checkpoint: LedgerCheckpointRef,
    pub goal_snapshot: Option<GoalSnapshotRef>,
    pub task_graph: TaskGraphRef,
    pub agent_snapshots: Vec<AgentResumeRef>,
    pub repository_specs: Vec<RepositoryRestoreSpec>,
    pub workspace_changes: Vec<ChangeSetRef>,
    pub context_state: ContextResumeRef,
    pub knowledge_refs: Vec<KnowledgeId>,
    pub artifact_manifest: ArtifactManifestRef,
    pub required_capabilities: CapabilityIntentSet,
    pub required_secrets: Vec<SecretRequirement>,
    pub target_constraints: WorkerConstraints,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub digest: Digest,
    pub signature: Signature,
}
```

### Explicitly excluded

- live CapabilityLease objects;
- plaintext API keys/tokens/cookies;
- browser profile secrets unless user explicitly selects an approved encrypted transfer mode;
- opaque local absolute paths without a portable repository/artifact mapping;
- hidden model chain-of-thought.

## 5. Restore algorithm

1. Verify bundle signature, digest, expiry, schema compatibility and source identity.
2. Verify target worker attestation against `WorkerConstraints`.
3. Create a fresh sandbox/workspace.
4. Clone/fetch exact repository revisions or restore content-addressed repo snapshots.
5. Apply each ChangeSet using preimage hashes; reject mismatch rather than fuzzy-apply silently.
6. Restore artifacts and Context Engine read/freshness metadata by digest.
7. Rebuild projections from ledger checkpoint plus tail events.
8. Recreate agent snapshots as `paused`; persistent background agents may be re-warmed after resume.
9. Re-run target policy using `CapabilityIntentSet`; issue no leases yet.
10. Commit execution-lease generation transfer.
11. Resume only if handoff command/policy explicitly authorized auto-resume; otherwise expose a paused ready session.

## 6. Interfaces

```rust
pub trait HandoffService {
    async fn prepare(&self, req: PrepareHandoff) -> Result<HandoffId>;
    async fn export_bundle(&self, id: HandoffId) -> Result<ArtifactRef>;
    async fn accept(&self, bundle: ArtifactRef, target: WorkerId) -> Result<RestorePlan>;
    async fn commit(&self, id: HandoffId, restore: RestoreId) -> Result<ExecutionGeneration>;
    async fn abort(&self, id: HandoffId) -> Result<()>;
}
```

CLI examples:

```text
/handoff remote --worker-pool secure-linux
/handoff macos --continue
rapid handoff session_01H... ssh://builder.example
rapid attach session_01H...
```

## 7. Failure modes

| Failure | Required behavior |
|---|---|
| target unavailable | source stays parked; user may resume source or choose target |
| schema mismatch | fail before lease transfer; provide upgrade/downgrade guidance |
| Git/repo revision unavailable | restore from approved content-addressed snapshot or block |
| patch preimage mismatch | block restore and surface path/hash conflict |
| target policy denies needed capability | restore as blocked/paused; never weaken policy |
| bundle expires | require fresh prepare; reject replay |
| source dies after target restored but before lease commit | consensus/lease store decides ownership by committed generation; target cannot self-promote |
| network partition | both sides fail closed for new writes unless they can prove current lease ownership |
| secret unavailable on target | task is blocked until target-specific secret handle is supplied |

## 8. Security

- Bundle is content-addressed, signed and encrypted in transit; sensitive artifacts can be separately encrypted at rest.
- Source trust does not imply target trust. Target worker identity/attestation and data residency constraints are checked.
- Handoff manifests may contain code; telemetry never copies raw payloads.
- Capability intents describe what future actions may be required; target policy evaluates them anew.
- Uncommitted changes are previewed before cross-host export when they include paths marked sensitive.
- Replay attacks are prevented by handoff ID, expiry, generation and committed execution lease.

## 9. Computer Use handoff

Browser/desktop sessions are normally **recreated**, not memory-migrated. The bundle can include:

- application startup recipe;
- current URL/app identifier;
- sanitized storage-state artifact if explicitly allowed;
- last accessibility/DOM state hash for comparison only;
- pending UI test plan;
- evidence/recording artifacts already captured.

A target always generates a fresh Observation before acting.

## 10. Example

```rust
let id = handoff.prepare(PrepareHandoff {
    session_id,
    target: WorkerSelector::Pool("linux-gvisor".into()),
    auto_resume: false,
}).await?;

let bundle = handoff.export_bundle(id).await?;
let restore = remote.accept(bundle).await?;
let generation = handoff.commit(id, restore.id).await?;
assert_eq!(generation.value(), old_generation + 1);
```

## 11. Acceptance evidence

- local→remote transfer preserves uncommitted patch and goal/evidence references;
- source cannot dispatch a write tool after transfer generation commits;
- target cannot use any source CapabilityLease;
- corrupted artifact/ChangeSet blocks before target activation;
- fault injection at every transfer phase never leaves two valid writers;
- remote→local return handoff works using the same protocol;
- computer-use target re-observes before any action.


## 12. Component diagram and implementation notes

```mermaid
sequenceDiagram
  participant S as Source Runtime
  participant L as ExecutionLease Store
  participant A as Artifact Transfer
  participant T as Target Runtime
  S->>L: mark generation N handoff_pending
  S->>S: quiesce side effects + checkpoint
  S->>A: signed HandoffBundle
  A->>T: transfer bundle/artifacts
  T->>T: compatibility + preimage verification
  T->>L: compare-and-swap N -> N+1 owner=target
  L-->>T: committed
  T->>T: restore parked state, then explicitly resume
  L-->>S: generation changed; source remains non-owner
```

Implementation rule: the linearization point is the durable lease-generation compare-and-swap, not network transfer completion. Every executor checks the current generation before side effects. Handoff transports may retry content-addressed artifacts, but lease transfer itself uses one idempotency key and must never be interpreted twice.
