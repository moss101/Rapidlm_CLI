# ADR — Observable trajectory learning

**Status:** Accepted for V2  
**Date:** 2026-08-14

## Context

V2 research identified a production need not fully covered by the V1 architecture.

## Decision

Harness trajectories may be used for evaluation/optimization while explicitly excluding any requirement to capture hidden chain-of-thought.

## Rationale

Muse demonstrates harness trajectory value; RapidLM keeps the data model provider-neutral, privacy-governed and based on observable events.

## Consequences

- Public contracts and events must be versioned and covered by deterministic replay/evals.
- Security boundaries continue to be enforced by Capability Broker/Sandbox rather than prompt convention.
- The implementation tasks in `prompts.md` are normative execution work for this ADR.
