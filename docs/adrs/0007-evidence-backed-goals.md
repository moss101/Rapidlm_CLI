# ADR 0007 — Evidence-backed autonomous goals

**Status:** Accepted for v1 baseline  
**Decision:** Represent goal lifecycle and completion criteria in runtime state; completion requires validated evidence.

## Context

Natural-language “done” is unreliable; autonomy must be budgeted/recoverable/auditable.

## Consequences

Active goal restores paused after crash; subagents cannot mutate top-level lifecycle.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
