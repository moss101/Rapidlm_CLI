# ADR 0004 — Graph-native orchestration

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Runtime Graph is canonical orchestration authority.

## Rationale

Dynamic parallel/recovery/invalidation cannot depend on model memory or linear coordinator loop.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
