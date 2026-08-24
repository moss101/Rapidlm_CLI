# Architecture — LLM Router and Provider Runtime

## 1. Responsibility
Route each Agent/Verifier node across cloud/local providers under capability, quality, latency, privacy, cost and observed harness-reliability constraints.

## 2. Non-negotiable design rules
- Hard constraints remove candidates before scoring.
- Routing decisions are versioned/observable, not hidden reasoning.
- Fallback never weakens privacy/capability constraints.

## 3. Components
- **ProviderRegistry** — adapter capabilities/health
- **ModelCatalog** — context/tools/vision/structured properties
- **RoutePolicy** — hard filters + scored choice
- **CircuitBreaker** — provider failure state
- **UsageAccounting** — tokens/cost/latency

## 4. Canonical contracts
`RouteRequest`, `ModelCapabilities`, `RouteDecision`, `FallbackPolicy`, `ProviderUsage`.

## 5. Failure and recovery
Provider transient failure follows bounded fallback policy; permanent/auth/billing errors park affected work. Context overflow requests compaction/reroute based on graph policy.

## 6. Security and trust
Privacy/data-residency and secret/provider scopes are hard constraints. Provider errors/tool content remain untrusted boundaries.

## 7. Implementation notes
Include measured tool-call repair/error rate and task-class eval score in optional routing features. Support user/model pinning and offline/local routes.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
