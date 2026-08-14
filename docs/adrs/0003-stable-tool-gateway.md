# ADR 0003 — Stable narrow model tool gateway

**Status:** Accepted for v1 baseline  
**Decision:** Expose a small stable tool catalog and hide dynamic providers/plugins/MCP behind gateways.

## Context

Reduces model schema tokens, prompt-cache churn and authorization surface.

## Consequences

Capability discovery stays runtime-side; tool result envelopes are bounded/versioned.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
