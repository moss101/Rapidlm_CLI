# ADR 0005 — Action-bound capability leases

**Status:** Accepted for v1 baseline  
**Decision:** Authorization yields short-lived action-bound leases validated by executors.

## Context

A boolean approval cannot prevent confused-deputy or TOCTOU changes between UI approval and side effect.

## Consequences

Normalize before hashing; bind principal, scope, policy revision, expiry and uses.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
