# Knowledge, Playbook and Automation API Contract

```rust
pub struct KnowledgeQuery {
    pub actor: PrincipalRef,
    pub project: ProjectId,
    pub repos: Vec<RepoId>,
    pub task_summary: String,
    pub paths: Vec<RepoPath>,
    pub tags: Vec<String>,
    pub max_items: u16,
    pub max_tokens: u32,
}

pub struct KnowledgeHit {
    pub id: KnowledgeId,
    pub title: String,
    pub body: String,
    pub score: f32,
    pub trigger_reasons: Vec<String>,
    pub freshness: Freshness,
    pub evidence: Vec<EvidenceSourceRef>,
}

pub struct PlaybookRunRequest {
    pub playbook: PlaybookRef,
    pub inputs: JsonValue,
    pub actor: PrincipalRef,
    pub policy_profile: PolicyProfileRef,
    pub trigger: TriggerRef,
}
```

Rules:

- Knowledge cannot grant capabilities;
- only approved/non-deprecated Knowledge is injected by default;
- Playbook publication validates DAG acyclicity and schema;
- automation triggers are untrusted data and are deduplicated by idempotency key;
- scheduled/non-interactive runs block when policy returns `ask` unless pre-authorized policy explicitly covers the exact capability class.
