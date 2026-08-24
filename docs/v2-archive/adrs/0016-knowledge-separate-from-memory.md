# ADR — Knowledge is a distinct governed registry

**Status:** Accepted for V2  
**Date:** 2026-08-14

## Context

V2 research identified a production need not fully covered by the V1 architecture.

## Decision

Knowledge, Memory, Rules, Skills, Playbooks and Policy remain separate domain concepts with different authority/lifecycle.

## Rationale

Trigger-scoped knowledge is useful, but conflating durable facts with episodic memory or security rules creates ambiguity.

## Consequences

- Public contracts and events must be versioned and covered by deterministic replay/evals.
- Security boundaries continue to be enforced by Capability Broker/Sandbox rather than prompt convention.
- The implementation tasks in `prompts.md` are normative execution work for this ADR.
