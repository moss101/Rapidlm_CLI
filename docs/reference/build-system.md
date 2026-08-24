# Build System Specification

The existing repository build is source of truth. V3 target remains Rust-first runtime with workspace-scoped crates and a separate TypeScript SDK/schema package. Phase 0 records exact current compiler/package-manager pins before any upgrade. CI uses targeted package/crate commands for developer speed and full integration gates for release. Generated protocol/SDK bindings are checked for drift. Release builds are reproducible enough to produce digest/SBOM/provenance/signature records and updater manifests.
