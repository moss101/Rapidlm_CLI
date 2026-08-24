# ADR 0015 — Wake-on-event background runtime

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Processes/jobs signal Monitor/Trigger nodes; models do not poll.

## Rationale

Polling wastes tokens/latency and complicates durability.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
