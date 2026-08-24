# Architecture — Telemetry, Trace, Metrics and Session Insights

## 1. Responsibility
Expose execution diagnostics and evidence-linked post-session improvement findings without making telemetry the durable product state.

## 2. Non-negotiable design rules
- Events are durable facts; traces diagnostic; metrics aggregated.
- Telemetry redacts at source and respects data policy.
- Insights propose experiments, never silently change production behavior.

## 3. Components
- **TraceService** — spans/events high-cardinality
- **Metrics** — bounded aggregated measurements
- **Exporter** — opt-in/managed sinks
- **SessionInsights** — deterministic heuristics + optional summary
- **DiagnosticsBundle** — support artifact

## 4. Canonical contracts
`TraceId/SpanId`, metric registry, `InsightFinding {category,evidence_refs,recommendation,candidate_experiment}`.

## 5. Failure and recovery
Exporter failure never blocks core task unless enterprise policy requires audit sink; local traces rotate by retention. Insights handle incomplete sessions explicitly.

## 6. Security and trust
Sensitive code/secrets classified/redacted; external telemetry egress policy-controlled and receipt-audited.

## 7. Implementation notes
Insights cover context waste, tool loops/repairs, agent topology, policy friction, verification gaps, Computer Use inefficiency, resource waste and recovery.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
