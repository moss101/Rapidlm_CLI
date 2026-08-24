# Build and CI

Phase 0 discovers current commands from `rust-toolchain.toml`, Cargo metadata, task files and CI before changing them. Target pipeline: formatting → clippy/static analysis → unit/contract → integration → deterministic replay → security → selected eval/perf → cross-platform release builds. Run affected crates/packages first; full workspace at integration gates. SDK generated schemas are diff-checked. Caches never affect correctness evidence.
