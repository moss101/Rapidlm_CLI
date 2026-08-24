# ADR 0004 — Hybrid structural/lexical/semantic context

**Status:** Accepted for v1 baseline  
**Decision:** Combine FTS, Tree-sitter, optional embeddings, symbol graph, read tracking and MMR.

## Context

No single retrieval method covers exact identifiers, semantics and code relationships; embeddings must remain optional.

## Consequences

Index is rebuildable; every context item carries provenance/freshness/token reason.

## Revisit when

Revisit only with measured evidence from production/evals or a protocol/platform constraint that invalidates the assumptions. A replacement ADR must describe migration and compatibility impact.
