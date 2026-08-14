# ADR 0011 — Provenance graph over Git

**Status:** Accepted for v1 baseline  
**Decision:** Keep Git interoperability while recording goal→evidence→change→verification attribution.

## Context

Git is ubiquitous but commit history alone lacks agent intent/evidence; replacing Git is unnecessary for v1.

## Consequences

Attestations reference ordinary commits/patches/artifacts and can be exported.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
