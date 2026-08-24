# Context Engineering — RapidLM V3

## 1. Objective

Context Engineering turns repository/project/session state into the **smallest complete evidence-bearing packet** that lets a model perform one graph node correctly. The metric is not maximum recall in isolation; it is verified progress per token under freshness, scope and trust constraints.

## 2. InformationNeed

Every non-trivial Agent/Verifier node may declare:

```rust
pub struct InformationNeed {
    pub questions: Vec<String>,
    pub scope: ScopeSet,
    pub known_anchors: Vec<CodeRef>,
    pub completeness: CompletenessRequirement,
    pub need_callers: bool,
    pub need_tests: bool,
    pub need_types: bool,
    pub need_config: bool,
    pub negative_claim_policy: NegativeClaimPolicy,
    pub token_budget: usize,
}
```

`CompletenessRequirement` distinguishes representative context from exhaustive enumeration. “Find every caller” therefore changes search/verification behavior.

## 3. Retrieval ladder

1. explicit file/path/symbol/line/attachment and current read set;
2. `rg` + FTS/BM25 exact/lexical;
3. Tree-sitter syntax/symbol extraction;
4. LSP definitions/references/types/diagnostics;
5. git/manifests/build/test failure links;
6. bounded code/knowledge graph expansion;
7. semantic/vector candidates when configured;
8. merge/rerank using lexical/semantic/graph distance/freshness/trust/task relevance;
9. MMR/equivalent diversity;
10. token-budget packing.

Deterministic extraction precedes LLM inference. Vector search is optional; offline lexical/structural mode remains functional.

## 4. Context Scout protocol

For unfamiliar/non-trivial areas, a cheap read-only Context Scout is preferred over having the expensive orchestrator repeatedly search. Scout tool surface is deliberately narrow: semantic/code retrieval, exact grep and file view (plus repository metadata where required).

Output:

```rust
pub struct ContextScoutReport {
    pub summary: String,
    pub scope: SearchScopeReport,
    pub references: Vec<Reference>,
    pub negative_findings: Vec<NegativeFinding>,
    pub open_questions: Vec<String>,
    pub snippets: Vec<SnippetRef>,
    pub search_log_ref: ArtifactId,
}
```

- `references` is exhaustive when the request requires enumeration.
- `snippets` are only the small number of locations the parent should read deeply.
- every negative finding includes checked scopes/patterns + confidence.
- zero hits must be broadened (repo scope + alternate spelling + plausible sibling/generated/vendor locations) before absence is claimed.
- scout instructions state **what to find**, not how to search; the scout prompt owns methodology.

## 5. Read tool as context compiler

`repo.read` uses independent ceilings:
- maximum requested line window;
- maximum bytes returned;
- maximum characters per line;
- estimated token cap.

Limits are dynamically reduced for small verifier/reviewer nodes and may expand for architecture exploration within the node budget. Truncation returns a structured continuation cursor and the exact reason; no ambiguous empty result.

`ReadObservation` records path identity, content hash, mtime/revision, ranges, full/partial/clamped state and context visibility generation.

## 6. Visibility lineage and dedup

Dedup asks: **is this exact content evidence currently reachable in this agent's active context?** It never asks only “was it read before?”. Every packet has `visibility_generation`. Compaction/rebuild invalidates prior visibility refs that are no longer present. Therefore a later read can resend content when the prior copy has fallen out of context.

## 7. Cross-tool freshness

Before write, patch or completion-critical claim, referenced reads/preimages must still match the current workspace revision. Writes emit invalidation events for affected:
- read observations;
- ContextPackets;
- code graph entities/edges;
- test-selection cache;
- evidence/verification based on old hashes.

## 8. ContextPacket

```rust
pub struct ContextItem {
    pub kind: ContextItemKind,
    pub source: ProvenanceRef,
    pub content_hash: Digest,
    pub revision: RevisionRef,
    pub retrieval_method: RetrievalMethod,
    pub reason: String,
    pub trust: TrustClass,
    pub estimated_tokens: u32,
    pub freshness: Freshness,
}

pub struct ContextPacket {
    pub packet_id: ContextPacketId,
    pub visibility_generation: u64,
    pub items: Vec<ContextItem>,
    pub omitted: ContextOmissionStats,
    pub total_tokens: u32,
    pub budget: u32,
}
```

## 9. Budgeting/compaction

Budget order reserves output first. Protected items: core/role rules, current user task, goal/criteria, current node contract, active blockers, relevant evidence obligations, required skills. Reclaim order: duplicate raw output → superseded/stale reads → old verbose tool output replaced by artifact refs → older conversation detail → structured summary. Deterministic fallback summary exists if summarizer model fails.

Compaction is observable and versioned; a model-generated summary is evidence-linked but not treated as repository truth.

## 10. `/context` inspector

Shows tokens by system/tools/rules/goal/current node/retrieved code/evidence/history/output reserve; each item has reason, method, revision/hash and freshness. It also shows reclaimable tokens, dedup savings, stale items discarded and retrieval latency.

## 11. Context evaluation

Metrics: precision@k, evidence recall, exhaustive-reference recall, negative-claim error, stale-context rate, duplicate-token ratio, tokens/success, context compile latency, cache hit correctness, read repetition, post-write invalidation accuracy and local-model success delta.
