# ADR 0006 — Transactional isolated workspace views

**Status:** Accepted for v1 baseline  
**Decision:** Give parallel write agents isolated views and stage merges transactionally.

## Context

Shared checkout writes create race/data-loss risks and make provenance ambiguous.

## Consequences

Git worktree is default parallel backend; overlay/remote backends implement same contract.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
