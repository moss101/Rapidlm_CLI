# ADR 0001 — Rust-first core with TypeScript SDK

**Status:** Accepted for v1 baseline  
**Decision:** Use Rust for kernel/TUI/execution/persistence and expose a transport-level TypeScript SDK.

## Context

Single-binary startup, process/sandbox/filesystem safety and long-lived async work favor Rust; ecosystem integrations favor TypeScript.

## Consequences

Do not duplicate business logic in SDK; generate wire types and contract fixtures.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
