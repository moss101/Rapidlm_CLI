# ADR 0009 — WASM as plugin execution default

**Status:** Accepted for v1 baseline  
**Decision:** Run third-party code extensions under capability-scoped WASM; keep skills declarative and hooks out of process.

## Context

Native in-process plugins would expand the trusted computing base and crash/security blast radius.

## Consequences

Native helpers require explicit external process policy, not implicit plugin privilege.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
