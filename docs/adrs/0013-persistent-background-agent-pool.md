# ADR — Persistent session-long background agents

**Status:** Accepted for V2  
**Date:** 2026-08-14

## Context

V2 research identified a production need not fully covered by the V1 architecture.

## Decision

RapidLM will support bounded read-only persistent background agents for recurring exploration/context work. They communicate via typed mailboxes and never share ambient write access.

## Rationale

Muse Code demonstrates latency/context benefits from session-long async background agents; typed boundaries preserve RapidLM security and inspectability.

## Consequences

- Public contracts and events must be versioned and covered by deterministic replay/evals.
- Security boundaries continue to be enforced by Capability Broker/Sandbox rather than prompt convention.
- The implementation tasks in `prompts.md` are normative execution work for this ADR.
