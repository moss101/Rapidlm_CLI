# Provider catalog import and refresh

This design compares the pinned reference trees, not their current upstream heads.
The requested OpenCode fork is at `77bba6a46ac6aeb98362a55d017c8f83ba6f22eb`;
Kimi Code is at `743f140978bddaf8b6aa524765fcd578f7c22fc9`.

The OpenCode fork's `packages/opencode/src/provider/models.ts` loads a cached
models.dev snapshot, falls back to a bundled copy, and refreshes the public JSON
with a ten-second timeout; `cli/cmd/models.ts` exposes explicit refresh. Its
schema includes provider and model IDs, endpoint and SDK identity, limits,
modalities, lifecycle, optional model-level provider, cost and capabilities.

Kimi's `packages/kosong/src/catalog.ts` resolves an explicit wire type first,
then known SDK identities, and distinguishes rejected, missing-endpoint and
guessed OpenAI-compatible results. It filters deprecated, alpha, embedding and
non-text-output entries, retains reasoning options and per-model protocol/API
overrides, and adapts Anthropic endpoints. `packages/node-sdk/src/catalog.ts`
implements the public fetch and alias mapping. The agent service's
`packages/agent-core-v2/src/app/kosongConfig/modelsDevUpstream.ts` has a ten-minute
cache, a ten-second timeout, in-flight coalescing and stale/bundled fallback;
`modelsDevImportService.ts` serializes writes, rejects OAuth-owned provider
replacement, reconciles credentials, removes stale imported rows, and seeds a
default only when unset. These source observations do not verify live providers.

RapidLM already has immutable validated snapshots in `crates/llm-router/src/catalog.rs`,
but user configuration and the terminal use explicit model profiles and do not
currently import or refresh a registry. REF-012 requires a bounded, provenance-bearing
source and persisted snapshot; explicit credential and endpoint binding; stale/offline
behavior; explicit import and refresh controls in CLI and terminal; migration guidance;
and diagnostics for removed models. A refresh must not silently replace a configured
default, credential, or user-authored model. Catalog-provided URLs, headers and SDK names
are untrusted metadata and must not authorize credential transmission on their own.
The source and credential decision should be user-owned, model and endpoint changes
must be reviewable, and concurrent writes must preserve unrelated configuration.
