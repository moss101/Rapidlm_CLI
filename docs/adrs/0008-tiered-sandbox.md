# ADR 0008 — Tiered execution isolation

**Status:** Accepted for v1 baseline  
**Decision:** Offer host-restricted, rootless container, gVisor and remote microVM tiers.

## Context

No single sandbox works across latency, portability and hostile-code isolation requirements.

## Consequences

Required tier never silently downgrades; network is independently policy-controlled.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
