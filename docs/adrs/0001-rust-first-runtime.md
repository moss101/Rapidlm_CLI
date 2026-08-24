# ADR 0001 — Rust-first runtime

**Status:** Accepted for V3 baseline  
**Date:** 2026-08-24

## Decision

Use Rust for kernel/TUI/control plane; SDK/tooling may use TypeScript.

## Rationale

Process/fs/network/sandbox/SQLite/concurrency benefit from memory safety and single-binary distribution.

## Consequences

Implementation tasks must preserve this decision unless a superseding ADR records new evidence, migration and compatibility/security impact.
