# Architecture — Build, Packaging, Updates and Supply Chain

## 1. Responsibility
Deliver reproducible, signed cross-platform binaries and SDK artifacts with explicit toolchain pins, dependency auditing, SBOM and rollback.

## 2. Non-negotiable design rules
- Runtime remains Rust-first/single-binary oriented.
- Build tools are pinned/hermetic where practical.
- Release provenance and update channels are signed.

## 3. Components
- **CargoWorkspace** — runtime crates/apps
- **SDKWorkspace** — TypeScript SDK/schema tooling
- **CI** — format/lint/test/eval/security/build
- **ReleasePipeline** — cross-platform sign/package
- **Updater** — channel/manifest/signature/rollback

## 4. Canonical contracts
`ReleaseManifest`, `BuildProvenance`, `SBOM`, `UpdateChannel`. Toolchain upgrades are explicit tasks with compatibility evidence.

## 5. Failure and recovery
Failed partial release cannot publish a mixed manifest. Updater validates signatures/digest before swap and retains rollback path.

## 6. Security and trust
Dependency/license/vulnerability scans, locked dependencies, least-privilege CI tokens, secret-free logs and provenance attestations.

## 7. Implementation notes
Initial V3 migration retains existing V2 project pins until Phase 0 audit; do not guess newer versions merely because dossier date advanced.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
