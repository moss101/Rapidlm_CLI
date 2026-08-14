# ADR 0002 — Event-sourced durable sessions

**Status:** Accepted for v1 baseline  
**Decision:** Make an append-only session event ledger the authoritative runtime history.

## Context

Crash recovery, replay, debugging, audit, UI projections and headless streaming need one ordered history.

## Consequences

SQLite WAL is primary durability; projections/checkpoints are rebuildable.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
