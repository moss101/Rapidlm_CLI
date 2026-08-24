# ADR 0010 — MCP for external capabilities, ACP for client integration

**Status:** Accepted for v1 baseline  
**Decision:** Use MCP behind external capability gateway and ACP as a frontend transport to the kernel.

## Context

Protocols solve different boundaries; neither should become the core runtime architecture.

## Consequences

Target MCP 2026-07-28 and ACP v1/v2 negotiation at initial implementation.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
