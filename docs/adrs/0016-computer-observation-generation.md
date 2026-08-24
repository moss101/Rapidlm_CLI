# ADR 0016 — Generation-bound Computer Use

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Every input uses fresh Observation generation and coordinate normalization.

## Rationale

Prevents stale GUI assumptions from causing wrong actions.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
