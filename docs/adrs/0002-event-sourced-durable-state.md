# ADR 0002 — Event-sourced durable state

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Use append-only Event Ledger plus projections/checkpoints.

## Rationale

Replay/audit/resume require durable facts independent of UI/model transcript.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
