# ADR 0009 — Cross-tool invariant engine

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Validate relational state such as fresh read before write.

## Rationale

Some correctness invariants span otherwise-valid tool calls.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
