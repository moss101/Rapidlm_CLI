# ADR 0012 — Content-minimized telemetry

**Status:** Accepted for v1 baseline  
**Decision:** Diagnostics are local-first and OpenTelemetry-compatible; raw content is off by default.

## Context

Enterprise/dev-tool observability is required, but code/prompts/secrets are high-sensitivity.

## Consequences

Explicit diagnostic exports are redacted and previewable.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
