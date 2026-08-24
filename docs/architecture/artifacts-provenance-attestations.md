# Architecture — Artifacts, Provenance, Change Attribution and Attestations

## 1. Responsibility
Provide content-addressed storage and graph references for large outputs, diffs, screenshots/video, reports, binaries, deployments, traces and verification evidence.

## 2. Non-negotiable design rules
- Large data lives in CAS, not Event Ledger/prompt.
- Artifact identity is digest-based and provenance-linked.
- Completion evidence records exact artifact revision/digest.

## 3. Components
- **ArtifactStore** — CAS read/write/GC
- **ArtifactRegistry** — typed metadata
- **ProvenanceService** — producer/run/node/source edges
- **AttestationService** — signed verification/release claims
- **RetentionPolicy** — scope-aware lifecycle

## 4. Canonical contracts
`ArtifactId=sha256`, `ArtifactMeta {type,mime,size,producer_node,goal,trust,retention}`, `ProvenanceEdge`, `Attestation`.

## 5. Failure and recovery
Interrupted uploads use temp staging + atomic commit. Missing/corrupt digest invalidates referencing evidence. GC preserves reachable/pinned/audit-required artifacts.

## 6. Security and trust
Secrets and sensitive outputs have restricted retention/export. Attestation signing keys never enter model context.

## 7. Implementation notes
Artifact UI/JSONL uses handles and previews. Types include file/patch/log/test-report/screenshot/video/binary/SBOM/trace/graph/deployment.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
