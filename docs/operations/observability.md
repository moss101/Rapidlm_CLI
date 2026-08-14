# Observability and Diagnostics

Use OpenTelemetry-compatible trace/span/metric concepts with a local implementation that works without a collector. Correlate `session_id`, `turn_id`, `agent_id`, `parent_agent_id`, `agent_lineage_id`, `job_id`, `tool_call_id`, `model_call_id`, `workspace_view_id`, `execution_generation`, `control_lease_id`, `observation_id`, and `trace_id`.

Metrics: startup/render/event latency; index/retrieval latency and context tokens by source; model latency/usage/cost; tool duration/error; approval/deny rate; sandbox startup; agent queue/idle time; per-agent tokens/cost/tool calls; background-agent context growth; cross-agent duplicated context; coordinator overhead; parallel efficiency; merge-conflict cost; goal usage/budget; handoff duration/failure/ownership generation; takeover duration; Knowledge trigger precision/false-positive rate; Playbook/Automation success/idempotency; trajectory export/redaction counts; Session Insight categories; Computer Use semantic-target rate, vision/coordinate fallback, stale-action rejection, screenshot/vision tokens, sensitive-action violations, assertion/evidence completeness, recording bytes; scanner duration/findings.

Default telemetry excludes prompt text, code, shell output, file contents, screenshots/video, URLs with query strings, secret values and hidden chain-of-thought. Local trace artifacts may contain project-sensitive data only under explicit diagnostic mode and carry retention/redaction labels.

`rapid doctor` reports component health and safe metadata. `rapid trace export` produces a redacted bundle with manifest so users can inspect exactly what will be shared.


### V2 ownership diagnostics

Every side-effect event records the execution generation and controller identity. Diagnostics MUST make split-brain attempts, stale ControlLease use and stale Computer Use ObservationId failures visible without exposing protected content. `rapid insights analyze` operates on these observable events/metrics and produces evidence-linked recommendations rather than inferred private reasoning.
