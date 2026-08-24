# ADR — Managed clean-context workers

**Status:** Accepted for V2  
**Date:** 2026-08-14

## Context

V2 research identified a production need not fully covered by the V1 architecture.

## Decision

Write-capable delegated work runs in isolated managed workers created from TaskEnvelope rather than cloned parent transcript.

## Rationale

Devin-style clean-slate workers reduce context pollution; isolated views prevent conflicting parallel writes.

## Consequences

- Public contracts and events must be versioned and covered by deterministic replay/evals.
- Security boundaries continue to be enforced by Capability Broker/Sandbox rather than prompt convention.
- The implementation tasks in `prompts.md` are normative execution work for this ADR.
