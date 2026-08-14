# ADR — Computer Use is an evidence-producing platform subsystem

**Status:** Accepted for V2  
**Date:** 2026-08-14

## Context

V2 research identified a production need not fully covered by the V1 architecture.

## Decision

Browser, desktop, TUI and emulator interaction share a unified observe/act/verify contract with accessibility-first targeting and recorded evidence.

## Rationale

Devin demonstrates full-desktop testing/video value; RapidLM adds capability leases, semantic targeting and Goal evidence integration.

## Consequences

- Public contracts and events must be versioned and covered by deterministic replay/evals.
- Security boundaries continue to be enforced by Capability Broker/Sandbox rather than prompt convention.
- The implementation tasks in `prompts.md` are normative execution work for this ADR.
