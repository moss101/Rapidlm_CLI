# ADR 0008 — Validator-driven tool repair

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Validate original, localized repair only after failure, revalidate.

## Rationale

Improves smaller-model reliability without silently rewriting valid calls.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
