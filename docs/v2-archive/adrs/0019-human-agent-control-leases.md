# ADR — Exclusive human/agent control leases

**Status:** Accepted for V2  
**Date:** 2026-08-14

## Context

V2 research identified a production need not fully covered by the V1 architecture.

## Decision

Mutable workspace/input domains have explicit human-or-agent control ownership.

## Rationale

Human takeover is valuable but unsafe if modeled only as UI state; a lease makes concurrent-write prevention a kernel invariant.

## Consequences

- Public contracts and events must be versioned and covered by deterministic replay/evals.
- Security boundaries continue to be enforced by Capability Broker/Sandbox rather than prompt convention.
- The implementation tasks in `prompts.md` are normative execution work for this ADR.
