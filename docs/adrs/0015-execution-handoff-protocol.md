# ADR — Provider-neutral execution handoff

**Status:** Accepted for V2  
**Date:** 2026-08-14

## Context

V2 research identified a production need not fully covered by the V1 architecture.

## Decision

Sessions can migrate between compatible local/remote runtimes through signed HandoffBundles and an exclusive SessionExecutionLease generation transfer.

## Rationale

Devin validates local→cloud handoff as a useful workflow; RapidLM requires portability and anti-split-brain semantics.

## Consequences

- Public contracts and events must be versioned and covered by deterministic replay/evals.
- Security boundaries continue to be enforced by Capability Broker/Sandbox rather than prompt convention.
- The implementation tasks in `prompts.md` are normative execution work for this ADR.
