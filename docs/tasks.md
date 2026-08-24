# RapidLM V3 — Complete Development Task Breakdown

**Atomic tasks including phase gates:** 412

Each task maps 1:1 to `prompts.md` and `development-ledger.md`. Phase gates require all phase acceptance evidence, not merely task checkboxes. Phase 0 audits the actual repository before migration assumptions become code.

## P0 — Repository Audit & V2→V3 Migration

### P0-001 — Audit repository tree and current V2 implementation
- **Goal:** Implement or migrate `Audit repository tree and current V2 implementation` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-001`

### P0-002 — Map crates/modules to V3 ownership
- **Goal:** Implement or migrate `Map crates/modules to V3 ownership` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** P0-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-002`

### P0-003 — Inventory CLI commands and TUI surfaces
- **Goal:** Implement or migrate `Inventory CLI commands and TUI surfaces` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** P0-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-003`

### P0-004 — Inventory persistence schemas and migrations
- **Goal:** Implement or migrate `Inventory persistence schemas and migrations` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** P0-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-004`

### P0-005 — Inventory Event Ledger and side-effect journal behavior
- **Goal:** Implement or migrate `Inventory Event Ledger and side-effect journal behavior` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-005`

### P0-006 — Inventory tool contracts and schema versions
- **Goal:** Implement or migrate `Inventory tool contracts and schema versions` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-006`

### P0-007 — Inventory policy/approval/lease enforcement points
- **Goal:** Implement or migrate `Inventory policy/approval/lease enforcement points` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-007`

### P0-008 — Inventory sandbox/process/remote worker backends
- **Goal:** Implement or migrate `Inventory sandbox/process/remote worker backends` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-008`

### P0-009 — Inventory context/index/search implementation
- **Goal:** Implement or migrate `Inventory context/index/search implementation` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-009`

### P0-010 — Inventory goal/agent/workflow/DAG implementation
- **Goal:** Implement or migrate `Inventory goal/agent/workflow/DAG implementation` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-010`

### P0-011 — Inventory workspace/VCS/patch paths
- **Goal:** Implement or migrate `Inventory workspace/VCS/patch paths` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-011`

### P0-012 — Inventory browser/computer/mobile implementation
- **Goal:** Implement or migrate `Inventory browser/computer/mobile implementation` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-012`

### P0-013 — Inventory provider/model routing
- **Goal:** Implement or migrate `Inventory provider/model routing` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-013`

### P0-014 — Inventory MCP/ACP/hooks/plugins/skills
- **Goal:** Implement or migrate `Inventory MCP/ACP/hooks/plugins/skills` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-014`

### P0-015 — Inventory daemon/background/cron/handoff implementation
- **Goal:** Implement or migrate `Inventory daemon/background/cron/handoff implementation` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-015`

### P0-016 — Inventory build/test/lint/security/release configuration
- **Goal:** Implement or migrate `Inventory build/test/lint/security/release configuration` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-016`

### P0-017 — Run baseline unit/integration/eval suites
- **Goal:** Implement or migrate `Run baseline unit/integration/eval suites` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-017`

### P0-018 — Capture baseline startup/token/context/tool-repair metrics
- **Goal:** Implement or migrate `Capture baseline startup/token/context/tool-repair metrics` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-018`

### P0-019 — Create KEEP/ADAPT/REPLACE/DELETE migration matrix
- **Goal:** Implement or migrate `Create KEEP/ADAPT/REPLACE/DELETE migration matrix` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-019`

### P0-020 — Create V2→V3 ownership/dependency ADR
- **Goal:** Implement or migrate `Create V2→V3 ownership/dependency ADR` as the smallest production-ready change consistent with repository truth.
- **References:** `README.md`, `SDD.md`, `docs/research/migration-v2-to-v3.md`
- **Dependencies:** none
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p0-020`

### P0-GATE — Pass Repository Audit & V2→V3 Migration phase gate
- **Dependencies:** all 20 P0 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P1 — Kernel, Durability, Artifacts & Sessions

### P1-001 — Define canonical V3 IDs and error envelopes
- **Goal:** Implement or migrate `Define canonical V3 IDs and error envelopes` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-001`

### P1-002 — Version EventEnvelope and event-kind registry
- **Goal:** Implement or migrate `Version EventEnvelope and event-kind registry` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE, P1-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-002`

### P1-003 — Implement transactional session sequence allocation
- **Goal:** Implement or migrate `Implement transactional session sequence allocation` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE, P1-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-003`

### P1-004 — Implement event append + projection transaction
- **Goal:** Implement or migrate `Implement event append + projection transaction` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE, P1-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-004`

### P1-005 — Implement projection checkpoints and rebuild
- **Goal:** Implement or migrate `Implement projection checkpoints and rebuild` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-005`

### P1-006 — Implement content-addressed Artifact Store
- **Goal:** Implement or migrate `Implement content-addressed Artifact Store` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-006`

### P1-007 — Implement Artifact metadata/provenance registry
- **Goal:** Implement or migrate `Implement Artifact metadata/provenance registry` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-007`

### P1-008 — Implement Operation Journal state machine
- **Goal:** Implement or migrate `Implement Operation Journal state machine` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-008`

### P1-009 — Implement stable effect fingerprints
- **Goal:** Implement or migrate `Implement stable effect fingerprints` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-009`

### P1-010 — Implement idempotency classes and replay policy
- **Goal:** Implement or migrate `Implement idempotency classes and replay policy` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-010`

### P1-011 — Implement uncertain-effect reconciliation interface
- **Goal:** Implement or migrate `Implement uncertain-effect reconciliation interface` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-011`

### P1-012 — Implement hash-linked egress attempt receipts
- **Goal:** Implement or migrate `Implement hash-linked egress attempt receipts` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-012`

### P1-013 — Implement run/session lifecycle service
- **Goal:** Implement or migrate `Implement run/session lifecycle service` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-013`

### P1-014 — Implement graph/run checkpoint envelope
- **Goal:** Implement or migrate `Implement graph/run checkpoint envelope` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-014`

### P1-015 — Implement session recovery procedure
- **Goal:** Implement or migrate `Implement session recovery procedure` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-015`

### P1-016 — Implement active-goal recovery-to-paused rule
- **Goal:** Implement or migrate `Implement active-goal recovery-to-paused rule` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-016`

### P1-017 — Implement session fork lineage
- **Goal:** Implement or migrate `Implement session fork lineage` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-017`

### P1-018 — Implement checkpoint/rewind metadata
- **Goal:** Implement or migrate `Implement checkpoint/rewind metadata` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-018`

### P1-019 — Implement retention/GC roots for artifacts
- **Goal:** Implement or migrate `Implement retention/GC roots for artifacts` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-019`

### P1-020 — Implement redaction classes and secure logging
- **Goal:** Implement or migrate `Implement redaction classes and secure logging` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-020`

### P1-021 — Implement durable approval/wait records
- **Goal:** Implement or migrate `Implement durable approval/wait records` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-021`

### P1-022 — Implement kernel health snapshots
- **Goal:** Implement or migrate `Implement kernel health snapshots` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-022`

### P1-023 — Implement structured shutdown/quiesce
- **Goal:** Implement or migrate `Implement structured shutdown/quiesce` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-023`

### P1-024 — Add crash-at-every-transition durability suite
- **Goal:** Implement or migrate `Add crash-at-every-transition durability suite` as the smallest production-ready change consistent with repository truth.
- **References:** `SDD.md`, `docs/architecture/event-ledger-operation-journal.md`, `docs/architecture/kernel-runtime.md`, `docs/architecture/artifacts-provenance-attestations.md`
- **Dependencies:** P0-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p1-024`

### P1-GATE — Pass Kernel, Durability, Artifacts & Sessions phase gate
- **Dependencies:** all 24 P1 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P2 — Dynamic Runtime Graph

### P2-001 — Define RuntimeGraph/Node/Edge IR
- **Goal:** Implement or migrate `Define RuntimeGraph/Node/Edge IR` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-001`

### P2-002 — Define NodeKind and EdgeKind registries
- **Goal:** Implement or migrate `Define NodeKind and EdgeKind registries` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE, P2-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-002`

### P2-003 — Define node state machine
- **Goal:** Implement or migrate `Define node state machine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE, P2-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-003`

### P2-004 — Define GraphProposal contract
- **Goal:** Implement or migrate `Define GraphProposal contract` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE, P2-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-004`

### P2-005 — Implement graph proposal schema validation
- **Goal:** Implement or migrate `Implement graph proposal schema validation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-005`

### P2-006 — Implement dependency cycle validation
- **Goal:** Implement or migrate `Implement dependency cycle validation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-006`

### P2-007 — Implement graph revision persistence
- **Goal:** Implement or migrate `Implement graph revision persistence` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-007`

### P2-008 — Implement immutable graph diff model
- **Goal:** Implement or migrate `Implement immutable graph diff model` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-008`

### P2-009 — Implement host graph mutation service
- **Goal:** Implement or migrate `Implement host graph mutation service` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-009`

### P2-010 — Implement deterministic ready-set computation
- **Goal:** Implement or migrate `Implement deterministic ready-set computation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-010`

### P2-011 — Implement dependency condition evaluation
- **Goal:** Implement or migrate `Implement dependency condition evaluation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-011`

### P2-012 — Implement joins and barriers
- **Goal:** Implement or migrate `Implement joins and barriers` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-012`

### P2-013 — Implement shard/fan-out nodes
- **Goal:** Implement or migrate `Implement shard/fan-out nodes` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-013`

### P2-014 — Implement resource/capability readiness constraints
- **Goal:** Implement or migrate `Implement resource/capability readiness constraints` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-014`

### P2-015 — Implement workspace write-conflict readiness
- **Goal:** Implement or migrate `Implement workspace write-conflict readiness` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-015`

### P2-016 — Implement node attempt lifecycle
- **Goal:** Implement or migrate `Implement node attempt lifecycle` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-016`

### P2-017 — Implement node retry classes/backoff
- **Goal:** Implement or migrate `Implement node retry classes/backoff` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-017`

### P2-018 — Implement cancellation propagation
- **Goal:** Implement or migrate `Implement cancellation propagation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-018`

### P2-019 — Implement waiting/suspend/resume nodes
- **Goal:** Implement or migrate `Implement waiting/suspend/resume nodes` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-019`

### P2-020 — Implement approval node executor
- **Goal:** Implement or migrate `Implement approval node executor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-020`

### P2-021 — Implement AskUser node executor
- **Goal:** Implement or migrate `Implement AskUser node executor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-021`

### P2-022 — Implement trigger/timer node executor
- **Goal:** Implement or migrate `Implement trigger/timer node executor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-022`

### P2-023 — Implement graph checkpoints/resume
- **Goal:** Implement or migrate `Implement graph checkpoints/resume` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-023`

### P2-024 — Implement repair/replan revision semantics
- **Goal:** Implement or migrate `Implement repair/replan revision semantics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-024`

### P2-025 — Implement supersession semantics
- **Goal:** Implement or migrate `Implement supersession semantics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-025`

### P2-026 — Implement invalidation engine
- **Goal:** Implement or migrate `Implement invalidation engine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-026`

### P2-027 — Implement bounded invalidation propagation
- **Goal:** Implement or migrate `Implement bounded invalidation propagation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-027`

### P2-028 — Implement graph budget accounting
- **Goal:** Implement or migrate `Implement graph budget accounting` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-028`

### P2-029 — Implement scheduler fairness across runs
- **Goal:** Implement or migrate `Implement scheduler fairness across runs` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-029`

### P2-030 — Implement graph why-ready/why-blocked explain
- **Goal:** Implement or migrate `Implement graph why-ready/why-blocked explain` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-030`

### P2-031 — Implement graph export JSON/DOT/Mermaid
- **Goal:** Implement or migrate `Implement graph export JSON/DOT/Mermaid` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-031`

### P2-032 — Implement Graph Inspector kernel API
- **Goal:** Implement or migrate `Implement Graph Inspector kernel API` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/graph-engineering.md`, `docs/api-contracts/graph-runtime-api.md`
- **Dependencies:** P1-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p2-032`

### P2-GATE — Pass Dynamic Runtime Graph phase gate
- **Dependencies:** all 32 P2 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P3 — Context Fabric, Code Intelligence & Knowledge

### P3-001 — Define InformationNeed and ContextPacket contracts
- **Goal:** Implement or migrate `Define InformationNeed and ContextPacket contracts` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-001`

### P3-002 — Implement content hashing/file watcher generations
- **Goal:** Implement or migrate `Implement content hashing/file watcher generations` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE, P3-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-002`

### P3-003 — Implement language detection and chunking
- **Goal:** Implement or migrate `Implement language detection and chunking` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE, P3-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-003`

### P3-004 — Integrate ripgrep exact/regex search
- **Goal:** Implement or migrate `Integrate ripgrep exact/regex search` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE, P3-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-004`

### P3-005 — Implement SQLite FTS/BM25 index
- **Goal:** Implement or migrate `Implement SQLite FTS/BM25 index` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-005`

### P3-006 — Integrate Tree-sitter parser/symbol extractor
- **Goal:** Implement or migrate `Integrate Tree-sitter parser/symbol extractor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-006`

### P3-007 — Integrate LSP definitions/references/types/diagnostics
- **Goal:** Implement or migrate `Integrate LSP definitions/references/types/diagnostics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-007`

### P3-008 — Implement git/manifest/build/test link extractor
- **Goal:** Implement or migrate `Implement git/manifest/build/test link extractor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-008`

### P3-009 — Implement deterministic code graph projection
- **Goal:** Implement or migrate `Implement deterministic code graph projection` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-009`

### P3-010 — Implement bounded graph neighbor/impact queries
- **Goal:** Implement or migrate `Implement bounded graph neighbor/impact queries` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-010`

### P3-011 — Implement optional embedding/vector adapter
- **Goal:** Implement or migrate `Implement optional embedding/vector adapter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-011`

### P3-012 — Implement candidate merge/score normalization
- **Goal:** Implement or migrate `Implement candidate merge/score normalization` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-012`

### P3-013 — Implement MMR/diversity reranking
- **Goal:** Implement or migrate `Implement MMR/diversity reranking` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-013`

### P3-014 — Implement scope/trust/freshness filters
- **Goal:** Implement or migrate `Implement scope/trust/freshness filters` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-014`

### P3-015 — Implement token estimator/model tokenizer adapter
- **Goal:** Implement or migrate `Implement token estimator/model tokenizer adapter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-015`

### P3-016 — Implement context budget allocator
- **Goal:** Implement or migrate `Implement context budget allocator` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-016`

### P3-017 — Implement context packer with protected categories
- **Goal:** Implement or migrate `Implement context packer with protected categories` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-017`

### P3-018 — Implement Context Scout agent contract
- **Goal:** Implement or migrate `Implement Context Scout agent contract` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-018`

### P3-019 — Implement semantic-first scout retrieval flow
- **Goal:** Implement or migrate `Implement semantic-first scout retrieval flow` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-019`

### P3-020 — Implement exhaustive reference enumeration mode
- **Goal:** Implement or migrate `Implement exhaustive reference enumeration mode` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-020`

### P3-021 — Implement broaden-on-zero-hit policy
- **Goal:** Implement or migrate `Implement broaden-on-zero-hit policy` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-021`

### P3-022 — Implement checked negative findings
- **Goal:** Implement or migrate `Implement checked negative findings` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-022`

### P3-023 — Implement open-question handoff
- **Goal:** Implement or migrate `Implement open-question handoff` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-023`

### P3-024 — Implement read tool line/byte/per-line/token ceilings
- **Goal:** Implement or migrate `Implement read tool line/byte/per-line/token ceilings` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-024`

### P3-025 — Implement structured continuation cursors
- **Goal:** Implement or migrate `Implement structured continuation cursors` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-025`

### P3-026 — Implement ReadObservation ledger
- **Goal:** Implement or migrate `Implement ReadObservation ledger` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-026`

### P3-027 — Implement context visibility generations
- **Goal:** Implement or migrate `Implement context visibility generations` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-027`

### P3-028 — Implement context-aware dedup
- **Goal:** Implement or migrate `Implement context-aware dedup` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-028`

### P3-029 — Implement post-write context invalidation
- **Goal:** Implement or migrate `Implement post-write context invalidation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-029`

### P3-030 — Implement deterministic compaction fallback
- **Goal:** Implement or migrate `Implement deterministic compaction fallback` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-030`

### P3-031 — Implement model-assisted structured compaction
- **Goal:** Implement or migrate `Implement model-assisted structured compaction` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-031`

### P3-032 — Implement /context explainability metrics
- **Goal:** Implement or migrate `Implement /context explainability metrics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/context-engineering.md`, `docs/api-contracts/context-api.md`
- **Dependencies:** P2-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p3-032`

### P3-GATE — Pass Context Fabric, Code Intelligence & Knowledge phase gate
- **Dependencies:** all 32 P3 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

### P4-001 — Define WorkspaceView/WorkspaceTransaction contracts
- **Goal:** Implement or migrate `Define WorkspaceView/WorkspaceTransaction contracts` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-001`

### P4-002 — Implement direct workspace backend
- **Goal:** Implement or migrate `Implement direct workspace backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE, P4-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-002`

### P4-003 — Implement git-worktree backend
- **Goal:** Implement or migrate `Implement git-worktree backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE, P4-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-003`

### P4-004 — Implement overlay/sandbox backend
- **Goal:** Implement or migrate `Implement overlay/sandbox backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE, P4-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-004`

### P4-005 — Implement remote snapshot backend
- **Goal:** Implement or migrate `Implement remote snapshot backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-005`

### P4-006 — Implement preimage/hash guarded patch apply
- **Goal:** Implement or migrate `Implement preimage/hash guarded patch apply` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-006`

### P4-007 — Implement semantic patch metadata/intent
- **Goal:** Implement or migrate `Implement semantic patch metadata/intent` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-007`

### P4-008 — Implement external mutation detector
- **Goal:** Implement or migrate `Implement external mutation detector` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-008`

### P4-009 — Implement symbol-level change attribution
- **Goal:** Implement or migrate `Implement symbol-level change attribution` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-009`

### P4-010 — Implement child-view transactional integration
- **Goal:** Implement or migrate `Implement child-view transactional integration` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-010`

### P4-011 — Implement conflict detection/staging rollback
- **Goal:** Implement or migrate `Implement conflict detection/staging rollback` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-011`

### P4-012 — Implement read-before-write freshness invariant
- **Goal:** Implement or migrate `Implement read-before-write freshness invariant` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-012`

### P4-013 — Define stable model-visible Tool Gateway
- **Goal:** Implement or migrate `Define stable model-visible Tool Gateway` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-013`

### P4-014 — Define versioned tool schema registry
- **Goal:** Implement or migrate `Define versioned tool schema registry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-014`

### P4-015 — Implement validate-original-first tool parsing
- **Goal:** Implement or migrate `Implement validate-original-first tool parsing` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-015`

### P4-016 — Implement validator issue-path extraction
- **Goal:** Implement or migrate `Implement validator issue-path extraction` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-016`

### P4-017 — Implement optional-null repair rule
- **Goal:** Implement or migrate `Implement optional-null repair rule` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-017`

### P4-018 — Implement stringified-array/object repair rule
- **Goal:** Implement or migrate `Implement stringified-array/object repair rule` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-018`

### P4-019 — Implement singleton-array repair rule
- **Goal:** Implement or migrate `Implement singleton-array repair rule` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-019`

### P4-020 — Implement numeric/boolean/enum safe repair rules
- **Goal:** Implement or migrate `Implement numeric/boolean/enum safe repair rules` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-020`

### P4-021 — Implement semantic-ambiguity repair refusal
- **Goal:** Implement or migrate `Implement semantic-ambiguity repair refusal` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-021`

### P4-022 — Implement repair telemetry/model feedback
- **Goal:** Implement or migrate `Implement repair telemetry/model feedback` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-022`

### P4-023 — Implement universal ToolOutcome recovery contract
- **Goal:** Implement or migrate `Implement universal ToolOutcome recovery contract` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-023`

### P4-024 — Implement cross-tool invariant engine
- **Goal:** Implement or migrate `Implement cross-tool invariant engine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-024`

### P4-025 — Implement Capability vocabulary/normalization
- **Goal:** Implement or migrate `Implement Capability vocabulary/normalization` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-025`

### P4-026 — Implement policy precedence engine
- **Goal:** Implement or migrate `Implement policy precedence engine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-026`

### P4-027 — Implement CapabilityLease issuance/signature
- **Goal:** Implement or migrate `Implement CapabilityLease issuance/signature` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-027`

### P4-028 — Implement executor-side lease verification
- **Goal:** Implement or migrate `Implement executor-side lease verification` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-028`

### P4-029 — Implement durable approval queue
- **Goal:** Implement or migrate `Implement durable approval queue` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-029`

### P4-030 — Implement dont-ask automation semantics
- **Goal:** Implement or migrate `Implement dont-ask automation semantics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-030`

### P4-031 — Implement Capability Projection Engine
- **Goal:** Implement or migrate `Implement Capability Projection Engine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-031`

### P4-032 — Implement SecretHandle store
- **Goal:** Implement or migrate `Implement SecretHandle store` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-032`

### P4-033 — Implement Credential Broker ephemeral credentials
- **Goal:** Implement or migrate `Implement Credential Broker ephemeral credentials` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-033`

### P4-034 — Implement environment-identity-safe credential cache
- **Goal:** Implement or migrate `Implement environment-identity-safe credential cache` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/workspace-vcs-semantic-patch.md`, `docs/architecture/tool-contract-and-recovery.md`, `docs/architecture/capability-broker-policy-projection.md`
- **Dependencies:** P3-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p4-034`

### P4-GATE — Pass Workspace, Tool Gateway, Repair, Policy & Secrets phase gate
- **Dependencies:** all 34 P4 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P5 — Agent Harness, Prompts, Models & Skills

### P5-001 — Define AgentExecutionContext and AgentResult
- **Goal:** Implement or migrate `Define AgentExecutionContext and AgentResult` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-001`

### P5-002 — Implement node AgentExecutor interface
- **Goal:** Implement or migrate `Implement node AgentExecutor interface` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE, P5-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-002`

### P5-003 — Implement role registry and capability profiles
- **Goal:** Implement or migrate `Implement role registry and capability profiles` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE, P5-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-003`

### P5-004 — Implement main agent role
- **Goal:** Implement or migrate `Implement main agent role` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE, P5-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-004`

### P5-005 — Implement Context Scout role
- **Goal:** Implement or migrate `Implement Context Scout role` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-005`

### P5-006 — Implement planner/architect role
- **Goal:** Implement or migrate `Implement planner/architect role` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-006`

### P5-007 — Implement coder/debugger roles
- **Goal:** Implement or migrate `Implement coder/debugger roles` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-007`

### P5-008 — Implement reviewer/tester roles
- **Goal:** Implement or migrate `Implement reviewer/tester roles` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-008`

### P5-009 — Implement independent verifier role
- **Goal:** Implement or migrate `Implement independent verifier role` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-009`

### P5-010 — Implement security/performance reviewer roles
- **Goal:** Implement or migrate `Implement security/performance reviewer roles` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-010`

### P5-011 — Implement browser/computer operator role
- **Goal:** Implement or migrate `Implement browser/computer operator role` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-011`

### P5-012 — Implement release-manager role
- **Goal:** Implement or migrate `Implement release-manager role` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-012`

### P5-013 — Implement clean-context TaskEnvelope
- **Goal:** Implement or migrate `Implement clean-context TaskEnvelope` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-013`

### P5-014 — Implement typed agent mailbox/result refs
- **Goal:** Implement or migrate `Implement typed agent mailbox/result refs` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-014`

### P5-015 — Implement isolated subagent lifecycle
- **Goal:** Implement or migrate `Implement isolated subagent lifecycle` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-015`

### P5-016 — Implement persistent read-only specialist lifecycle
- **Goal:** Implement or migrate `Implement persistent read-only specialist lifecycle` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-016`

### P5-017 — Implement background specialist bounded summaries
- **Goal:** Implement or migrate `Implement background specialist bounded summaries` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-017`

### P5-018 — Implement delegation utility scoring
- **Goal:** Implement or migrate `Implement delegation utility scoring` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-018`

### P5-019 — Implement nested delegation depth/budget bounds
- **Goal:** Implement or migrate `Implement nested delegation depth/budget bounds` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-019`

### P5-020 — Implement agent cancellation/timeouts
- **Goal:** Implement or migrate `Implement agent cancellation/timeouts` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-020`

### P5-021 — Implement empty-response retry guard
- **Goal:** Implement or migrate `Implement empty-response retry guard` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-021`

### P5-022 — Implement repeated tool-call loop detector
- **Goal:** Implement or migrate `Implement repeated tool-call loop detector` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-022`

### P5-023 — Implement repeated-message/stream loop detector
- **Goal:** Implement or migrate `Implement repeated-message/stream loop detector` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-023`

### P5-024 — Implement compact-before-context-overflow retry
- **Goal:** Implement or migrate `Implement compact-before-context-overflow retry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-024`

### P5-025 — Implement per-subtask model routing
- **Goal:** Implement or migrate `Implement per-subtask model routing` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-025`

### P5-026 — Implement provider/model capability profiles
- **Goal:** Implement or migrate `Implement provider/model capability profiles` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-026`

### P5-027 — Implement routing fallback/circuit breakers
- **Goal:** Implement or migrate `Implement routing fallback/circuit breakers` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-027`

### P5-028 — Implement PromptRegistry version/hash model
- **Goal:** Implement or migrate `Implement PromptRegistry version/hash model` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-028`

### P5-029 — Implement layered PromptComposer
- **Goal:** Implement or migrate `Implement layered PromptComposer` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-029`

### P5-030 — Implement AGENTS hierarchy loader
- **Goal:** Implement or migrate `Implement AGENTS hierarchy loader` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-030`

### P5-031 — Implement skill metadata registry
- **Goal:** Implement or migrate `Implement skill metadata registry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-031`

### P5-032 — Implement skill progressive disclosure
- **Goal:** Implement or migrate `Implement skill progressive disclosure` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/agent-harness.md`, `docs/architecture/prompt-runtime.md`, `system-prompts/system-prompt-composition.md`
- **Dependencies:** P4-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p5-032`

### P5-GATE — Pass Agent Harness, Prompts, Models & Skills phase gate
- **Dependencies:** all 32 P5 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P6 — Goals, Evidence & Independent Verification

### P6-001 — Define GoalContract and lifecycle
- **Goal:** Implement or migrate `Define GoalContract and lifecycle` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-001`

### P6-002 — Implement active/paused/blocked goal state
- **Goal:** Implement or migrate `Implement active/paused/blocked goal state` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE, P6-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-002`

### P6-003 — Implement goal create/replace/clear rules
- **Goal:** Implement or migrate `Implement goal create/replace/clear rules` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE, P6-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-003`

### P6-004 — Implement goal turn-boundary context injection
- **Goal:** Implement or migrate `Implement goal turn-boundary context injection` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE, P6-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-004`

### P6-005 — Implement explicit turn/token/wall-clock budgets
- **Goal:** Implement or migrate `Implement explicit turn/token/wall-clock budgets` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-005`

### P6-006 — Implement budget convergence guidance
- **Goal:** Implement or migrate `Implement budget convergence guidance` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-006`

### P6-007 — Implement goal crash recovery behavior
- **Goal:** Implement or migrate `Implement goal crash recovery behavior` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-007`

### P6-008 — Define Criterion/Claim/Evidence schemas
- **Goal:** Implement or migrate `Define Criterion/Claim/Evidence schemas` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-008`

### P6-009 — Implement evidence provenance/freshness
- **Goal:** Implement or migrate `Implement evidence provenance/freshness` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-009`

### P6-010 — Implement claim support/contradiction edges
- **Goal:** Implement or migrate `Implement claim support/contradiction edges` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-010`

### P6-011 — Implement evidence invalidation propagation
- **Goal:** Implement or migrate `Implement evidence invalidation propagation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-011`

### P6-012 — Define VerificationRecord/verdict
- **Goal:** Implement or migrate `Define VerificationRecord/verdict` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-012`

### P6-013 — Implement deterministic verifier adapters
- **Goal:** Implement or migrate `Implement deterministic verifier adapters` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-013`

### P6-014 — Implement independent verifier Agent node
- **Goal:** Implement or migrate `Implement independent verifier Agent node` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-014`

### P6-015 — Implement verifier context isolation
- **Goal:** Implement or migrate `Implement verifier context isolation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-015`

### P6-016 — Implement CompletionCandidate contract
- **Goal:** Implement or migrate `Implement CompletionCandidate contract` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-016`

### P6-017 — Implement host GoalCompletionGate
- **Goal:** Implement or migrate `Implement host GoalCompletionGate` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-017`

### P6-018 — Implement false-completion rejection path
- **Goal:** Implement or migrate `Implement false-completion rejection path` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-018`

### P6-019 — Implement rejection gap→repair graph expansion
- **Goal:** Implement or migrate `Implement rejection gap→repair graph expansion` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-019`

### P6-020 — Implement visual vs deterministic proof precedence
- **Goal:** Implement or migrate `Implement visual vs deterministic proof precedence` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-020`

### P6-021 — Implement goal pause/resume/cancel kernel APIs
- **Goal:** Implement or migrate `Implement goal pause/resume/cancel kernel APIs` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-021`

### P6-022 — Implement goal/evidence TUI inspector
- **Goal:** Implement or migrate `Implement goal/evidence TUI inspector` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-022`

### P6-023 — Implement goal export/attestation
- **Goal:** Implement or migrate `Implement goal export/attestation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-023`

### P6-024 — Add adversarial false-completion eval suite
- **Goal:** Implement or migrate `Add adversarial false-completion eval suite` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/goals-evidence-verification.md`, `docs/api-contracts/goal-evidence-api.md`
- **Dependencies:** P5-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p6-024`

### P6-GATE — Pass Goals, Evidence & Independent Verification phase gate
- **Dependencies:** all 24 P6 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

### P7-001 — Define ProcessSpec/ProcessHandle/ProcessOutcome
- **Goal:** Implement or migrate `Define ProcessSpec/ProcessHandle/ProcessOutcome` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-001`

### P7-002 — Implement process group/tree ownership
- **Goal:** Implement or migrate `Implement process group/tree ownership` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE, P7-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-002`

### P7-003 — Implement PTY sessions and input generation guards
- **Goal:** Implement or migrate `Implement PTY sessions and input generation guards` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE, P7-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-003`

### P7-004 — Implement bounded stdout/stderr head-tail results
- **Goal:** Implement or migrate `Implement bounded stdout/stderr head-tail results` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE, P7-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-004`

### P7-005 — Implement full process log Artifact spooling
- **Goal:** Implement or migrate `Implement full process log Artifact spooling` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-005`

### P7-006 — Implement foreground→daemon detach policy
- **Goal:** Implement or migrate `Implement foreground→daemon detach policy` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-006`

### P7-007 — Implement process timeout/cancellation
- **Goal:** Implement or migrate `Implement process timeout/cancellation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-007`

### P7-008 — Implement protected-user-process safeguards
- **Goal:** Implement or migrate `Implement protected-user-process safeguards` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-008`

### P7-009 — Define MonitorSpec and event predicates
- **Goal:** Implement or migrate `Define MonitorSpec and event predicates` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-009`

### P7-010 — Implement exit-code monitor
- **Goal:** Implement or migrate `Implement exit-code monitor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-010`

### P7-011 — Implement regex/log-event monitor
- **Goal:** Implement or migrate `Implement regex/log-event monitor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-011`

### P7-012 — Implement port/readiness monitor
- **Goal:** Implement or migrate `Implement port/readiness monitor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-012`

### P7-013 — Implement file/event monitor
- **Goal:** Implement or migrate `Implement file/event monitor` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-013`

### P7-014 — Implement graph wake-on-monitor events
- **Goal:** Implement or migrate `Implement graph wake-on-monitor events` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-014`

### P7-015 — Define TriggerSpec and durable cursors
- **Goal:** Implement or migrate `Define TriggerSpec and durable cursors` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-015`

### P7-016 — Implement cron trigger parser/scheduler
- **Goal:** Implement or migrate `Implement cron trigger parser/scheduler` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-016`

### P7-017 — Implement deterministic per-job jitter
- **Goal:** Implement or migrate `Implement deterministic per-job jitter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-017`

### P7-018 — Implement JobLease generation/heartbeat
- **Goal:** Implement or migrate `Implement JobLease generation/heartbeat` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-018`

### P7-019 — Implement missed one-shot semantics
- **Goal:** Implement or migrate `Implement missed one-shot semantics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-019`

### P7-020 — Define SandboxSpec/backend trait
- **Goal:** Implement or migrate `Define SandboxSpec/backend trait` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-020`

### P7-021 — Implement host-restricted backend
- **Goal:** Implement or migrate `Implement host-restricted backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-021`

### P7-022 — Implement rootless/container backend
- **Goal:** Implement or migrate `Implement rootless/container backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-022`

### P7-023 — Implement stronger kernel/microVM adapter seam
- **Goal:** Implement or migrate `Implement stronger kernel/microVM adapter seam` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-023`

### P7-024 — Define ResourcePool/EnvironmentLease
- **Goal:** Implement or migrate `Define ResourcePool/EnvironmentLease` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-024`

### P7-025 — Implement warm acquire/release
- **Goal:** Implement or migrate `Implement warm acquire/release` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-025`

### P7-026 — Implement environment phase telemetry
- **Goal:** Implement or migrate `Implement environment phase telemetry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-026`

### P7-027 — Implement pool sanitation/quarantine
- **Goal:** Implement or migrate `Implement pool sanitation/quarantine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-027`

### P7-028 — Implement pool miss fallback provisioning
- **Goal:** Implement or migrate `Implement pool miss fallback provisioning` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-028`

### P7-029 — Implement remote worker resource backend
- **Goal:** Implement or migrate `Implement remote worker resource backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-029`

### P7-030 — Add disconnect/install-interruption chaos tests
- **Goal:** Implement or migrate `Add disconnect/install-interruption chaos tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/process-background-trigger-runtime.md`, `docs/architecture/sandbox-resource-pool.md`
- **Dependencies:** P6-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p7-030`

### P7-GATE — Pass Process, Sandbox, Resource Pool, Jobs & Triggers phase gate
- **Dependencies:** all 30 P7 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P8 — Browser, Computer Use, Mobile & Preview

### P8-001 — Define SurfaceRef/Observation/generation contracts
- **Goal:** Implement or migrate `Define SurfaceRef/Observation/generation contracts` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-001`

### P8-002 — Define ComputerAction schema
- **Goal:** Implement or migrate `Define ComputerAction schema` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE, P8-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-002`

### P8-003 — Implement coordinate normalization/scaling
- **Goal:** Implement or migrate `Implement coordinate normalization/scaling` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE, P8-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-003`

### P8-004 — Implement pointer state/move/click/down/up
- **Goal:** Implement or migrate `Implement pointer state/move/click/down/up` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE, P8-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-004`

### P8-005 — Implement double-click and drag
- **Goal:** Implement or migrate `Implement double-click and drag` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-005`

### P8-006 — Implement keyboard key/chord/type
- **Goal:** Implement or migrate `Implement keyboard key/chord/type` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-006`

### P8-007 — Implement scroll and wait conditions
- **Goal:** Implement or migrate `Implement scroll and wait conditions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-007`

### P8-008 — Implement screenshot/region/cursor actions
- **Goal:** Implement or migrate `Implement screenshot/region/cursor actions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-008`

### P8-009 — Implement action batching rules
- **Goal:** Implement or migrate `Implement action batching rules` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-009`

### P8-010 — Implement settle/reobserve policy
- **Goal:** Implement or migrate `Implement settle/reobserve policy` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-010`

### P8-011 — Implement screenshot-on-failure diagnostics
- **Goal:** Implement or migrate `Implement screenshot-on-failure diagnostics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-011`

### P8-012 — Implement action metrics/trace events
- **Goal:** Implement or migrate `Implement action metrics/trace events` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-012`

### P8-013 — Implement BrowserSession lifecycle
- **Goal:** Implement or migrate `Implement BrowserSession lifecycle` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-013`

### P8-014 — Implement DOM/test-id locator actions
- **Goal:** Implement or migrate `Implement DOM/test-id locator actions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-014`

### P8-015 — Implement accessibility-tree browser actions
- **Goal:** Implement or migrate `Implement accessibility-tree browser actions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-015`

### P8-016 — Implement CDP allowlist bridge
- **Goal:** Implement or migrate `Implement CDP allowlist bridge` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-016`

### P8-017 — Implement browser console/network capture
- **Goal:** Implement or migrate `Implement browser console/network capture` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-017`

### P8-018 — Implement browser JS execution policy gate
- **Goal:** Implement or migrate `Implement browser JS execution policy gate` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-018`

### P8-019 — Implement download/upload policy paths
- **Goal:** Implement or migrate `Implement download/upload policy paths` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-019`

### P8-020 — Implement prompt-injection fencing for page content
- **Goal:** Implement or migrate `Implement prompt-injection fencing for page content` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-020`

### P8-021 — Implement macOS AX backend
- **Goal:** Implement or migrate `Implement macOS AX backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-021`

### P8-022 — Implement Windows UIA backend
- **Goal:** Implement or migrate `Implement Windows UIA backend` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-022`

### P8-023 — Implement Linux AT-SPI/platform pointer seam
- **Goal:** Implement or migrate `Implement Linux AT-SPI/platform pointer seam` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-023`

### P8-024 — Implement PreviewSupervisor lifecycle
- **Goal:** Implement or migrate `Implement PreviewSupervisor lifecycle` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-024`

### P8-025 — Implement dynamic port/readiness checks
- **Goal:** Implement or migrate `Implement dynamic port/readiness checks` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-025`

### P8-026 — Implement compiler/HMR/HTTP diagnostics
- **Goal:** Implement or migrate `Implement compiler/HMR/HTTP diagnostics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-026`

### P8-027 — Implement preview console/network feedback
- **Goal:** Implement or migrate `Implement preview console/network feedback` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-027`

### P8-028 — Implement visual evidence recorder
- **Goal:** Implement or migrate `Implement visual evidence recorder` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-028`

### P8-029 — Implement Android emulator/ADB adapter
- **Goal:** Implement or migrate `Implement Android emulator/ADB adapter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-029`

### P8-030 — Implement iOS simctl remote-mac adapter
- **Goal:** Implement or migrate `Implement iOS simctl remote-mac adapter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-030`

### P8-031 — Implement ControlLease human takeover
- **Goal:** Implement or migrate `Implement ControlLease human takeover` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-031`

### P8-032 — Implement post-takeover mutation/surface reconciliation
- **Goal:** Implement or migrate `Implement post-takeover mutation/surface reconciliation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/computer-use.md`, `docs/architecture/preview-visual-verification.md`, `docs/reference/computer-use-command-reference.md`
- **Dependencies:** P7-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p8-032`

### P8-GATE — Pass Browser, Computer Use, Mobile & Preview phase gate
- **Dependencies:** all 32 P8 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

### P9-001 — Define MCP catalog revision model
- **Goal:** Implement or migrate `Define MCP catalog revision model` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-001`

### P9-002 — Implement MCP client discovery/cache
- **Goal:** Implement or migrate `Implement MCP client discovery/cache` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE, P9-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-002`

### P9-003 — Implement MCP invocation middleware chain
- **Goal:** Implement or migrate `Implement MCP invocation middleware chain` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE, P9-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-003`

### P9-004 — Implement MCP output trust fencing
- **Goal:** Implement or migrate `Implement MCP output trust fencing` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE, P9-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-004`

### P9-005 — Implement explicit RapidLM MCP server surface
- **Goal:** Implement or migrate `Implement explicit RapidLM MCP server surface` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-005`

### P9-006 — Implement ACP stdio framing
- **Goal:** Implement or migrate `Implement ACP stdio framing` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-006`

### P9-007 — Implement ACP session/progress/diff/permission mapping
- **Goal:** Implement or migrate `Implement ACP session/progress/diff/permission mapping` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-007`

### P9-008 — Implement versioned headless JSONL schema
- **Goal:** Implement or migrate `Implement versioned headless JSONL schema` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-008`

### P9-009 — Implement JSONL stdout purity tests
- **Goal:** Implement or migrate `Implement JSONL stdout purity tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-009`

### P9-010 — Implement TypeScript SDK generated contracts
- **Goal:** Implement or migrate `Implement TypeScript SDK generated contracts` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-010`

### P9-011 — Implement SDK reconnect/event cursors
- **Goal:** Implement or migrate `Implement SDK reconnect/event cursors` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-011`

### P9-012 — Define lifecycle HookEvent registry
- **Goal:** Implement or migrate `Define lifecycle HookEvent registry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-012`

### P9-013 — Implement hook filter/matcher engine
- **Goal:** Implement or migrate `Implement hook filter/matcher engine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-013`

### P9-014 — Implement hook timeouts/process-group cleanup
- **Goal:** Implement or migrate `Implement hook timeouts/process-group cleanup` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-014`

### P9-015 — Implement per-event fail-open/fail-closed semantics
- **Goal:** Implement or migrate `Implement per-event fail-open/fail-closed semantics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-015`

### P9-016 — Implement hook capability restrictions
- **Goal:** Implement or migrate `Implement hook capability restrictions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-016`

### P9-017 — Define WASM plugin manifest/WIT host API
- **Goal:** Implement or migrate `Define WASM plugin manifest/WIT host API` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-017`

### P9-018 — Implement WASM capability intersection
- **Goal:** Implement or migrate `Implement WASM capability intersection` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-018`

### P9-019 — Implement plugin lifecycle/update/disable
- **Goal:** Implement or migrate `Implement plugin lifecycle/update/disable` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-019`

### P9-020 — Implement skill/plugin marketplace metadata seam
- **Goal:** Implement or migrate `Implement skill/plugin marketplace metadata seam` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-020`

### P9-021 — Implement Playbook→initial graph compiler
- **Goal:** Implement or migrate `Implement Playbook→initial graph compiler` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-021`

### P9-022 — Implement external ACP agent adapter
- **Goal:** Implement or migrate `Implement external ACP agent adapter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-022`

### P9-023 — Implement external CLI agent adapter
- **Goal:** Implement or migrate `Implement external CLI agent adapter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-023`

### P9-024 — Implement external agent result/evidence normalization
- **Goal:** Implement or migrate `Implement external agent result/evidence normalization` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-024`

### P9-025 — Define HandoffBundle
- **Goal:** Implement or migrate `Define HandoffBundle` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-025`

### P9-026 — Implement SessionExecutionLease generation transfer
- **Goal:** Implement or migrate `Implement SessionExecutionLease generation transfer` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-026`

### P9-027 — Implement local↔remote handoff state machine
- **Goal:** Implement or migrate `Implement local↔remote handoff state machine` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-027`

### P9-028 — Implement fresh target lease/credential/observation issuance
- **Goal:** Implement or migrate `Implement fresh target lease/credential/observation issuance` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/architecture/plugins-skills-hooks.md`, `docs/architecture/remote-workers-handoff-control.md`
- **Dependencies:** P8-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p9-028`

### P9-GATE — Pass MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff phase gate
- **Dependencies:** all 28 P9 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P10 — CLI, TUI, Daemon, Inspectors & Time Travel

### P10-001 — Implement rapid composition root/startup
- **Goal:** Implement or migrate `Implement rapid composition root/startup` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-001`

### P10-002 — Implement daemon IPC/authentication
- **Goal:** Implement or migrate `Implement daemon IPC/authentication` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE, P10-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-002`

### P10-003 — Implement KernelClient in-process/IPC transports
- **Goal:** Implement or migrate `Implement KernelClient in-process/IPC transports` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE, P10-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-003`

### P10-004 — Implement interactive prompt/transcript reducer
- **Goal:** Implement or migrate `Implement interactive prompt/transcript reducer` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE, P10-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-004`

### P10-005 — Implement streaming coalescing/backpressure
- **Goal:** Implement or migrate `Implement streaming coalescing/backpressure` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-005`

### P10-006 — Implement 80-column/NO_COLOR rendering
- **Goal:** Implement or migrate `Implement 80-column/NO_COLOR rendering` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-006`

### P10-007 — Implement command palette/slash command registry
- **Goal:** Implement or migrate `Implement command palette/slash command registry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-007`

### P10-008 — Implement graph inspector UI
- **Goal:** Implement or migrate `Implement graph inspector UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-008`

### P10-009 — Implement context inspector UI
- **Goal:** Implement or migrate `Implement context inspector UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-009`

### P10-010 — Implement agent inspector UI
- **Goal:** Implement or migrate `Implement agent inspector UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-010`

### P10-011 — Implement files/diff WorkspaceTransaction UI
- **Goal:** Implement or migrate `Implement files/diff WorkspaceTransaction UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-011`

### P10-012 — Implement terminal/process monitor UI
- **Goal:** Implement or migrate `Implement terminal/process monitor UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-012`

### P10-013 — Implement approvals UI
- **Goal:** Implement or migrate `Implement approvals UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-013`

### P10-014 — Implement evidence/verification UI
- **Goal:** Implement or migrate `Implement evidence/verification UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-014`

### P10-015 — Implement browser/computer panel
- **Goal:** Implement or migrate `Implement browser/computer panel` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-015`

### P10-016 — Implement resources/sandbox panel
- **Goal:** Implement or migrate `Implement resources/sandbox panel` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-016`

### P10-017 — Implement memory/knowledge/preference inspector
- **Goal:** Implement or migrate `Implement memory/knowledge/preference inspector` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-017`

### P10-018 — Implement trace/events inspector
- **Goal:** Implement or migrate `Implement trace/events inspector` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-018`

### P10-019 — Implement model/router selector/status
- **Goal:** Implement or migrate `Implement model/router selector/status` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-019`

### P10-020 — Implement goal card/budget controls
- **Goal:** Implement or migrate `Implement goal card/budget controls` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-020`

### P10-021 — Implement human-control ownership indicator
- **Goal:** Implement or migrate `Implement human-control ownership indicator` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-021`

### P10-022 — Implement session list/search/resume
- **Goal:** Implement or migrate `Implement session list/search/resume` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-022`

### P10-023 — Implement timeline/checkpoint UI
- **Goal:** Implement or migrate `Implement timeline/checkpoint UI` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-023`

### P10-024 — Implement conversation/graph/workspace rewind
- **Goal:** Implement or migrate `Implement conversation/graph/workspace rewind` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-024`

### P10-025 — Implement non-destructive fork from checkpoint
- **Goal:** Implement or migrate `Implement non-destructive fork from checkpoint` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-025`

### P10-026 — Implement headless rapid exec/run
- **Goal:** Implement or migrate `Implement headless rapid exec/run` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-026`

### P10-027 — Implement rapid inspect/export
- **Goal:** Implement or migrate `Implement rapid inspect/export` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-027`

### P10-028 — Implement rapid doctor diagnostics
- **Goal:** Implement or migrate `Implement rapid doctor diagnostics` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-028`

### P10-029 — Implement shell completion/man/help generation
- **Goal:** Implement or migrate `Implement shell completion/man/help generation` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-029`

### P10-030 — Benchmark startup/10k-event replay/render
- **Goal:** Implement or migrate `Benchmark startup/10k-event replay/render` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/cli-tui.md`, `docs/architecture/sessions-checkpoints-time-travel.md`
- **Dependencies:** P9-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p10-030`

### P10-GATE — Pass CLI, TUI, Daemon, Inspectors & Time Travel phase gate
- **Dependencies:** all 30 P10 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P11 — Eval Harness, Trajectories, Preferences & Optimization

### P11-001 — Define eval Scenario/Fixture schemas
- **Goal:** Implement or migrate `Define eval Scenario/Fixture schemas` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-001`

### P11-002 — Implement FixtureManager CAS repos/services
- **Goal:** Implement or migrate `Implement FixtureManager CAS repos/services` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE, P11-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-002`

### P11-003 — Implement ScriptedModel
- **Goal:** Implement or migrate `Implement ScriptedModel` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE, P11-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-003`

### P11-004 — Implement ReplayProvider
- **Goal:** Implement or migrate `Implement ReplayProvider` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE, P11-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-004`

### P11-005 — Implement LiveProvider experiment pinning
- **Goal:** Implement or migrate `Implement LiveProvider experiment pinning` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-005`

### P11-006 — Implement production KernelRunner modes
- **Goal:** Implement or migrate `Implement production KernelRunner modes` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-006`

### P11-007 — Implement FaultInjector framework
- **Goal:** Implement or migrate `Implement FaultInjector framework` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-007`

### P11-008 — Implement graph assertions
- **Goal:** Implement or migrate `Implement graph assertions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-008`

### P11-009 — Implement file/workspace assertions
- **Goal:** Implement or migrate `Implement file/workspace assertions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-009`

### P11-010 — Implement policy/capability assertions
- **Goal:** Implement or migrate `Implement policy/capability assertions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-010`

### P11-011 — Implement process/resource assertions
- **Goal:** Implement or migrate `Implement process/resource assertions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-011`

### P11-012 — Implement context/evidence assertions
- **Goal:** Implement or migrate `Implement context/evidence assertions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-012`

### P11-013 — Implement browser/computer assertions
- **Goal:** Implement or migrate `Implement browser/computer assertions` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-013`

### P11-014 — Implement deterministic grader registry
- **Goal:** Implement or migrate `Implement deterministic grader registry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-014`

### P11-015 — Implement optional rubric JudgeAdapter
- **Goal:** Implement or migrate `Implement optional rubric JudgeAdapter` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-015`

### P11-016 — Implement MetricCollector registry
- **Goal:** Implement or migrate `Implement MetricCollector registry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-016`

### P11-017 — Implement FailureBundler
- **Goal:** Implement or migrate `Implement FailureBundler` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-017`

### P11-018 — Implement TrajectoryCollector
- **Goal:** Implement or migrate `Implement TrajectoryCollector` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-018`

### P11-019 — Implement trajectory privacy classification
- **Goal:** Implement or migrate `Implement trajectory privacy classification` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-019`

### P11-020 — Implement CandidateRanker
- **Goal:** Implement or migrate `Implement CandidateRanker` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-020`

### P11-021 — Implement ExperimentRegistry
- **Goal:** Implement or migrate `Implement ExperimentRegistry` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-021`

### P11-022 — Define FeedbackEvent accept/reject/edit
- **Goal:** Implement or migrate `Define FeedbackEvent accept/reject/edit` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-022`

### P11-023 — Implement PreferenceCandidate extraction
- **Goal:** Implement or migrate `Implement PreferenceCandidate extraction` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-023`

### P11-024 — Implement preference confidence/scope/decay
- **Goal:** Implement or migrate `Implement preference confidence/scope/decay` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-024`

### P11-025 — Implement preference conflict handling
- **Goal:** Implement or migrate `Implement preference conflict handling` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-025`

### P11-026 — Implement Session Insights analyzers
- **Goal:** Implement or migrate `Implement Session Insights analyzers` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-026`

### P11-027 — Implement held-out promotion gates
- **Goal:** Implement or migrate `Implement held-out promotion gates` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-027`

### P11-028 — Implement 1h/4h/12h/24h endurance harness
- **Goal:** Implement or migrate `Implement 1h/4h/12h/24h endurance harness` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-028`

### P11-029 — Implement >=1000-tool-call release scenario
- **Goal:** Implement or migrate `Implement >=1000-tool-call release scenario` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-029`

### P11-030 — Implement verified-success-per-token benchmark
- **Goal:** Implement or migrate `Implement verified-success-per-token benchmark` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/eval-harness.md`, `docs/architecture/preference-learning-trajectory.md`
- **Dependencies:** P10-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p11-030`

### P11-GATE — Pass Eval Harness, Trajectories, Preferences & Optimization phase gate
- **Dependencies:** all 30 P11 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P12 — Security, Reliability & Adversarial Hardening

### P12-001 — Create V3 STRIDE/data-flow threat model
- **Goal:** Implement or migrate `Create V3 STRIDE/data-flow threat model` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-001`

### P12-002 — Implement project trust gating
- **Goal:** Implement or migrate `Implement project trust gating` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE, P12-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-002`

### P12-003 — Implement symlink/realpath path escape tests
- **Goal:** Implement or migrate `Implement symlink/realpath path escape tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE, P12-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-003`

### P12-004 — Implement prompt-injection trust tagging
- **Goal:** Implement or migrate `Implement prompt-injection trust tagging` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE, P12-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-004`

### P12-005 — Implement MCP/plugin/hook privilege-escalation tests
- **Goal:** Implement or migrate `Implement MCP/plugin/hook privilege-escalation tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-005`

### P12-006 — Implement secret redaction/exfiltration tests
- **Goal:** Implement or migrate `Implement secret redaction/exfiltration tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-006`

### P12-007 — Implement egress policy/receipt tests
- **Goal:** Implement or migrate `Implement egress policy/receipt tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-007`

### P12-008 — Implement approval bypass negative tests
- **Goal:** Implement or migrate `Implement approval bypass negative tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-008`

### P12-009 — Implement dont-ask no-hang tests
- **Goal:** Implement or migrate `Implement dont-ask no-hang tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-009`

### P12-010 — Implement stale CapabilityLease tests
- **Goal:** Implement or migrate `Implement stale CapabilityLease tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-010`

### P12-011 — Implement stale Observation coordinate tests
- **Goal:** Implement or migrate `Implement stale Observation coordinate tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-011`

### P12-012 — Implement stale ReadObservation write tests
- **Goal:** Implement or migrate `Implement stale ReadObservation write tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-012`

### P12-013 — Implement uncertain-effect double-send tests
- **Goal:** Implement or migrate `Implement uncertain-effect double-send tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-013`

### P12-014 — Implement sandbox downgrade refusal tests
- **Goal:** Implement or migrate `Implement sandbox downgrade refusal tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-014`

### P12-015 — Implement resource pool cross-tenant contamination tests
- **Goal:** Implement or migrate `Implement resource pool cross-tenant contamination tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-015`

### P12-016 — Implement credential cache identity isolation tests
- **Goal:** Implement or migrate `Implement credential cache identity isolation tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-016`

### P12-017 — Implement remote worker forged-result tests
- **Goal:** Implement or migrate `Implement remote worker forged-result tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-017`

### P12-018 — Implement handoff split-brain partition tests
- **Goal:** Implement or migrate `Implement handoff split-brain partition tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-018`

### P12-019 — Implement plugin WASM resource exhaustion tests
- **Goal:** Implement or migrate `Implement plugin WASM resource exhaustion tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-019`

### P12-020 — Implement process tree escape/cancellation tests
- **Goal:** Implement or migrate `Implement process tree escape/cancellation tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-020`

### P12-021 — Implement archive/path traversal tests
- **Goal:** Implement or migrate `Implement archive/path traversal tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-021`

### P12-022 — Implement dependency/lockfile integrity scans
- **Goal:** Implement or migrate `Implement dependency/lockfile integrity scans` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-022`

### P12-023 — Implement secret/dependency/SAST scanner nodes
- **Goal:** Implement or migrate `Implement secret/dependency/SAST scanner nodes` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-023`

### P12-024 — Implement fuzzers for graph/event/tool decoders
- **Goal:** Implement or migrate `Implement fuzzers for graph/event/tool decoders` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-024`

### P12-025 — Implement persistence corruption recovery tests
- **Goal:** Implement or migrate `Implement persistence corruption recovery tests` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-025`

### P12-026 — Run full adversarial security release gate
- **Goal:** Implement or migrate `Run full adversarial security release gate` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/security/threat-model.md`, `docs/architecture/security-scanners.md`
- **Dependencies:** P11-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p12-026`

### P12-GATE — Pass Security, Reliability & Adversarial Hardening phase gate
- **Dependencies:** all 26 P12 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.

## P13 — Build, Release, Documentation & GA Migration

### P13-001 — Audit/lock Rust toolchain and Cargo workspace
- **Goal:** Implement or migrate `Audit/lock Rust toolchain and Cargo workspace` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-001`

### P13-002 — Audit/lock Node/TypeScript SDK toolchain
- **Goal:** Implement or migrate `Audit/lock Node/TypeScript SDK toolchain` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE, P13-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-002`

### P13-003 — Implement targeted CI crate/package matrix
- **Goal:** Implement or migrate `Implement targeted CI crate/package matrix` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE, P13-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-003`

### P13-004 — Implement format/lint/static analysis gates
- **Goal:** Implement or migrate `Implement format/lint/static analysis gates` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE, P13-001
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-004`

### P13-005 — Implement unit/integration/e2e test matrix
- **Goal:** Implement or migrate `Implement unit/integration/e2e test matrix` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-005`

### P13-006 — Implement deterministic replay CI gate
- **Goal:** Implement or migrate `Implement deterministic replay CI gate` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-006`

### P13-007 — Implement token/performance regression gates
- **Goal:** Implement or migrate `Implement token/performance regression gates` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-007`

### P13-008 — Implement cross-platform macOS/Linux/Windows builds
- **Goal:** Implement or migrate `Implement cross-platform macOS/Linux/Windows builds` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-008`

### P13-009 — Implement optional remote mac/iOS test lane
- **Goal:** Implement or migrate `Implement optional remote mac/iOS test lane` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-009`

### P13-010 — Generate SBOM and dependency/license reports
- **Goal:** Implement or migrate `Generate SBOM and dependency/license reports` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-010`

### P13-011 — Implement artifact signing/provenance attestations
- **Goal:** Implement or migrate `Implement artifact signing/provenance attestations` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-011`

### P13-012 — Implement release manifest/update channels
- **Goal:** Implement or migrate `Implement release manifest/update channels` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-012`

### P13-013 — Implement updater signature/digest verification
- **Goal:** Implement or migrate `Implement updater signature/digest verification` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-013`

### P13-014 — Implement rollback/failed-update recovery
- **Goal:** Implement or migrate `Implement rollback/failed-update recovery` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-014`

### P13-015 — Implement migration of V2 Event/Goal/Workspace data
- **Goal:** Implement or migrate `Implement migration of V2 Event/Goal/Workspace data` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-015`

### P13-016 — Implement migration compatibility fixtures
- **Goal:** Implement or migrate `Implement migration compatibility fixtures` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-016`

### P13-017 — Implement CLI/API schema compatibility fixtures
- **Goal:** Implement or migrate `Implement CLI/API schema compatibility fixtures` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-017`

### P13-018 — Write operator incident-recovery runbook
- **Goal:** Implement or migrate `Write operator incident-recovery runbook` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-018`

### P13-019 — Write observability/SLO runbook
- **Goal:** Implement or migrate `Write observability/SLO runbook` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-019`

### P13-020 — Write complete user CLI/TUI reference
- **Goal:** Implement or migrate `Write complete user CLI/TUI reference` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-020`

### P13-021 — Write extension/SDK developer guide
- **Goal:** Implement or migrate `Write extension/SDK developer guide` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-021`

### P13-022 — Run requirements traceability audit
- **Goal:** Implement or migrate `Run requirements traceability audit` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-022`

### P13-023 — Run clean-install/upgrade/rollback acceptance
- **Goal:** Implement or migrate `Run clean-install/upgrade/rollback acceptance` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-023`

### P13-024 — Execute GA release gate and archive evidence
- **Goal:** Implement or migrate `Execute GA release gate and archive evidence` as the smallest production-ready change consistent with repository truth.
- **References:** `docs/architecture/build-release-supply-chain.md`, `docs/operations/build-and-ci.md`, `docs/operations/release-and-supply-chain.md`
- **Dependencies:** P12-GATE
- **Acceptance:** typed/source-backed contract; happy + negative + recovery behavior; persistence/policy implications handled; repository-native verification observed; evidence recorded.
- **Prompt:** `prompts.md#p13-024`

### P13-GATE — Pass Build, Release, Documentation & GA Migration phase gate
- **Dependencies:** all 24 P13 implementation tasks.
- **Acceptance:** phase-specific integration/eval/security gates green; migration/compatibility evidence current; no unresolved blocking contradiction.
