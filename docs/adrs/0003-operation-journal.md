# ADR 0003 — Separate operation journal

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Track external side effects with prepare/executing/committed/uncertain/reconcile.

## Rationale

Event history alone cannot guarantee safe replay of non-idempotent effects.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
