# Release and Supply-Chain Plan

Release cross-platform signed binaries for macOS arm64/x64, Linux x64/arm64 and Windows x64. Build from pinned Rust/Node/pnpm toolchains and lockfiles in isolated CI. Generate SBOM and provenance attestation for each artifact.

Stable updater flow: fetch signed manifest → verify root key/trust chain → select exact platform artifact → verify hash/signature → install atomically → retain one rollback version. Refuse version rollback unless user invokes an explicit recovery command.

Dependency policy: automated license/vulnerability checks; deny git dependencies without pinned commit; minimize proc-macro/native dependency surface in security-critical crates; review new crypto/sandbox/update dependencies manually; vendor only with provenance/license record.

Release gate consumes `requirements-traceability.md`, all contract/eval/security gates, migration tests from last two minor versions, fresh-install and upgrade smoke tests, and signature verification from a clean machine.
