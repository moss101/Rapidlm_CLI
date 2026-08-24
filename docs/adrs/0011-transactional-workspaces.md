# ADR 0011 — Transactional isolated WorkspaceViews

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Parallel writers use separate views; integration is preimage/conflict/verification guarded.

## Rationale

Safe parallelism and rollback require isolation/provenance.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
