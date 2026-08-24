# RapidLM V3 — Atomic Implementation Prompts

**Execution rule:** run each prompt in a fresh task context. Read `agents.md`, the matching `tasks.md` entry, exact reference docs, applicable skills, and repository source/tests/config before editing. Clear transient context after the task; preserve source/evidence/ledger/typed handoff only.

## Universal contract

- preserve working behavior unless the task explicitly migrates it;
- never create a parallel authority;
- prefer typed versioned contracts;
- classify persistence/replay/cancellation/security effects;
- use bounded outputs and untrusted-content fencing;
- run configured repository-native gates;
- completion-critical claims require independent evidence;
- update `development-ledger.md`.

<a id="p0-001"></a>
## P0-001 — Audit repository tree and current V2 implementation

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Audit repository tree and current V2 implementation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-002"></a>
## P0-002 — Map crates/modules to V3 ownership

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Map crates/modules to V3 ownership.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** P0-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-003"></a>
## P0-003 — Inventory CLI commands and TUI surfaces

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory CLI commands and TUI surfaces.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** P0-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-004"></a>
## P0-004 — Inventory persistence schemas and migrations

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory persistence schemas and migrations.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** P0-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-005"></a>
## P0-005 — Inventory Event Ledger and side-effect journal behavior

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory Event Ledger and side-effect journal behavior.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-006"></a>
## P0-006 — Inventory tool contracts and schema versions

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory tool contracts and schema versions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-007"></a>
## P0-007 — Inventory policy/approval/lease enforcement points

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory policy/approval/lease enforcement points.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-008"></a>
## P0-008 — Inventory sandbox/process/remote worker backends

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory sandbox/process/remote worker backends.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-009"></a>
## P0-009 — Inventory context/index/search implementation

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory context/index/search implementation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-010"></a>
## P0-010 — Inventory goal/agent/workflow/DAG implementation

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory goal/agent/workflow/DAG implementation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-011"></a>
## P0-011 — Inventory workspace/VCS/patch paths

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory workspace/VCS/patch paths.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-012"></a>
## P0-012 — Inventory browser/computer/mobile implementation

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory browser/computer/mobile implementation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-013"></a>
## P0-013 — Inventory provider/model routing

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory provider/model routing.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-014"></a>
## P0-014 — Inventory MCP/ACP/hooks/plugins/skills

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory MCP/ACP/hooks/plugins/skills.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-015"></a>
## P0-015 — Inventory daemon/background/cron/handoff implementation

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory daemon/background/cron/handoff implementation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-016"></a>
## P0-016 — Inventory build/test/lint/security/release configuration

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Inventory build/test/lint/security/release configuration.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-017"></a>
## P0-017 — Run baseline unit/integration/eval suites

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Run baseline unit/integration/eval suites.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-018"></a>
## P0-018 — Capture baseline startup/token/context/tool-repair metrics

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Capture baseline startup/token/context/tool-repair metrics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-019"></a>
## P0-019 — Create KEEP/ADAPT/REPLACE/DELETE migration matrix

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Create KEEP/ADAPT/REPLACE/DELETE migration matrix.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-020"></a>
## P0-020 — Create V2→V3 ownership/dependency ADR

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Create V2→V3 ownership/dependency ADR.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** none

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p0-gate"></a>
## P0-GATE — Pass Repository Audit & V2→V3 Migration phase gate

**Phase:** P0 — Repository Audit & V2→V3 Migration

**Objective:** Pass Repository Audit & V2→V3 Migration phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `README.md`
- `SDD.md`
- `docs/research/migration-v2-to-v3.md`

**Dependencies:** P0-001, P0-002, P0-003, P0-004, P0-005, P0-006, P0-007, P0-008, P0-009, P0-010, P0-011, P0-012, P0-013, P0-014, P0-015, P0-016, P0-017, P0-018, P0-019, P0-020

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-001"></a>
## P1-001 — Define canonical V3 IDs and error envelopes

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Define canonical V3 IDs and error envelopes.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-002"></a>
## P1-002 — Version EventEnvelope and event-kind registry

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Version EventEnvelope and event-kind registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE, P1-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-003"></a>
## P1-003 — Implement transactional session sequence allocation

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement transactional session sequence allocation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE, P1-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-004"></a>
## P1-004 — Implement event append + projection transaction

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement event append + projection transaction.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE, P1-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-005"></a>
## P1-005 — Implement projection checkpoints and rebuild

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement projection checkpoints and rebuild.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-006"></a>
## P1-006 — Implement content-addressed Artifact Store

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement content-addressed Artifact Store.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-007"></a>
## P1-007 — Implement Artifact metadata/provenance registry

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement Artifact metadata/provenance registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-008"></a>
## P1-008 — Implement Operation Journal state machine

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement Operation Journal state machine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-009"></a>
## P1-009 — Implement stable effect fingerprints

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement stable effect fingerprints.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-010"></a>
## P1-010 — Implement idempotency classes and replay policy

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement idempotency classes and replay policy.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-011"></a>
## P1-011 — Implement uncertain-effect reconciliation interface

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement uncertain-effect reconciliation interface.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-012"></a>
## P1-012 — Implement hash-linked egress attempt receipts

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement hash-linked egress attempt receipts.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-013"></a>
## P1-013 — Implement run/session lifecycle service

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement run/session lifecycle service.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-014"></a>
## P1-014 — Implement graph/run checkpoint envelope

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement graph/run checkpoint envelope.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-015"></a>
## P1-015 — Implement session recovery procedure

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement session recovery procedure.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-016"></a>
## P1-016 — Implement active-goal recovery-to-paused rule

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement active-goal recovery-to-paused rule.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-017"></a>
## P1-017 — Implement session fork lineage

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement session fork lineage.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-018"></a>
## P1-018 — Implement checkpoint/rewind metadata

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement checkpoint/rewind metadata.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-019"></a>
## P1-019 — Implement retention/GC roots for artifacts

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement retention/GC roots for artifacts.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-020"></a>
## P1-020 — Implement redaction classes and secure logging

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement redaction classes and secure logging.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-021"></a>
## P1-021 — Implement durable approval/wait records

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement durable approval/wait records.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-022"></a>
## P1-022 — Implement kernel health snapshots

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement kernel health snapshots.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-023"></a>
## P1-023 — Implement structured shutdown/quiesce

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Implement structured shutdown/quiesce.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-024"></a>
## P1-024 — Add crash-at-every-transition durability suite

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Add crash-at-every-transition durability suite.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P0-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p1-gate"></a>
## P1-GATE — Pass Kernel, Durability, Artifacts & Sessions phase gate

**Phase:** P1 — Kernel, Durability, Artifacts & Sessions

**Objective:** Pass Kernel, Durability, Artifacts & Sessions phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `SDD.md`
- `docs/architecture/event-ledger-operation-journal.md`
- `docs/architecture/kernel-runtime.md`
- `docs/architecture/artifacts-provenance-attestations.md`

**Dependencies:** P1-001, P1-002, P1-003, P1-004, P1-005, P1-006, P1-007, P1-008, P1-009, P1-010, P1-011, P1-012, P1-013, P1-014, P1-015, P1-016, P1-017, P1-018, P1-019, P1-020, P1-021, P1-022, P1-023, P1-024

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-001"></a>
## P2-001 — Define RuntimeGraph/Node/Edge IR

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Define RuntimeGraph/Node/Edge IR.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-002"></a>
## P2-002 — Define NodeKind and EdgeKind registries

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Define NodeKind and EdgeKind registries.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE, P2-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-003"></a>
## P2-003 — Define node state machine

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Define node state machine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE, P2-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-004"></a>
## P2-004 — Define GraphProposal contract

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Define GraphProposal contract.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE, P2-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-005"></a>
## P2-005 — Implement graph proposal schema validation

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement graph proposal schema validation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-006"></a>
## P2-006 — Implement dependency cycle validation

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement dependency cycle validation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-007"></a>
## P2-007 — Implement graph revision persistence

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement graph revision persistence.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-008"></a>
## P2-008 — Implement immutable graph diff model

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement immutable graph diff model.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-009"></a>
## P2-009 — Implement host graph mutation service

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement host graph mutation service.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-010"></a>
## P2-010 — Implement deterministic ready-set computation

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement deterministic ready-set computation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-011"></a>
## P2-011 — Implement dependency condition evaluation

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement dependency condition evaluation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-012"></a>
## P2-012 — Implement joins and barriers

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement joins and barriers.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-013"></a>
## P2-013 — Implement shard/fan-out nodes

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement shard/fan-out nodes.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-014"></a>
## P2-014 — Implement resource/capability readiness constraints

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement resource/capability readiness constraints.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-015"></a>
## P2-015 — Implement workspace write-conflict readiness

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement workspace write-conflict readiness.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-016"></a>
## P2-016 — Implement node attempt lifecycle

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement node attempt lifecycle.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-017"></a>
## P2-017 — Implement node retry classes/backoff

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement node retry classes/backoff.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-018"></a>
## P2-018 — Implement cancellation propagation

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement cancellation propagation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-019"></a>
## P2-019 — Implement waiting/suspend/resume nodes

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement waiting/suspend/resume nodes.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-020"></a>
## P2-020 — Implement approval node executor

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement approval node executor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-021"></a>
## P2-021 — Implement AskUser node executor

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement AskUser node executor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-022"></a>
## P2-022 — Implement trigger/timer node executor

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement trigger/timer node executor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-023"></a>
## P2-023 — Implement graph checkpoints/resume

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement graph checkpoints/resume.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-024"></a>
## P2-024 — Implement repair/replan revision semantics

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement repair/replan revision semantics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-025"></a>
## P2-025 — Implement supersession semantics

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement supersession semantics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-026"></a>
## P2-026 — Implement invalidation engine

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement invalidation engine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-027"></a>
## P2-027 — Implement bounded invalidation propagation

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement bounded invalidation propagation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-028"></a>
## P2-028 — Implement graph budget accounting

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement graph budget accounting.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-029"></a>
## P2-029 — Implement scheduler fairness across runs

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement scheduler fairness across runs.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-030"></a>
## P2-030 — Implement graph why-ready/why-blocked explain

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement graph why-ready/why-blocked explain.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-031"></a>
## P2-031 — Implement graph export JSON/DOT/Mermaid

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement graph export JSON/DOT/Mermaid.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-032"></a>
## P2-032 — Implement Graph Inspector kernel API

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Implement Graph Inspector kernel API.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P1-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p2-gate"></a>
## P2-GATE — Pass Dynamic Runtime Graph phase gate

**Phase:** P2 — Dynamic Runtime Graph

**Objective:** Pass Dynamic Runtime Graph phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/graph-engineering.md`
- `docs/api-contracts/graph-runtime-api.md`

**Dependencies:** P2-001, P2-002, P2-003, P2-004, P2-005, P2-006, P2-007, P2-008, P2-009, P2-010, P2-011, P2-012, P2-013, P2-014, P2-015, P2-016, P2-017, P2-018, P2-019, P2-020, P2-021, P2-022, P2-023, P2-024, P2-025, P2-026, P2-027, P2-028, P2-029, P2-030, P2-031, P2-032

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-001"></a>
## P3-001 — Define InformationNeed and ContextPacket contracts

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Define InformationNeed and ContextPacket contracts.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-002"></a>
## P3-002 — Implement content hashing/file watcher generations

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement content hashing/file watcher generations.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE, P3-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-003"></a>
## P3-003 — Implement language detection and chunking

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement language detection and chunking.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE, P3-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-004"></a>
## P3-004 — Integrate ripgrep exact/regex search

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Integrate ripgrep exact/regex search.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE, P3-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-005"></a>
## P3-005 — Implement SQLite FTS/BM25 index

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement SQLite FTS/BM25 index.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-006"></a>
## P3-006 — Integrate Tree-sitter parser/symbol extractor

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Integrate Tree-sitter parser/symbol extractor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-007"></a>
## P3-007 — Integrate LSP definitions/references/types/diagnostics

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Integrate LSP definitions/references/types/diagnostics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-008"></a>
## P3-008 — Implement git/manifest/build/test link extractor

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement git/manifest/build/test link extractor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-009"></a>
## P3-009 — Implement deterministic code graph projection

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement deterministic code graph projection.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-010"></a>
## P3-010 — Implement bounded graph neighbor/impact queries

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement bounded graph neighbor/impact queries.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-011"></a>
## P3-011 — Implement optional embedding/vector adapter

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement optional embedding/vector adapter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-012"></a>
## P3-012 — Implement candidate merge/score normalization

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement candidate merge/score normalization.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-013"></a>
## P3-013 — Implement MMR/diversity reranking

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement MMR/diversity reranking.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-014"></a>
## P3-014 — Implement scope/trust/freshness filters

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement scope/trust/freshness filters.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-015"></a>
## P3-015 — Implement token estimator/model tokenizer adapter

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement token estimator/model tokenizer adapter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-016"></a>
## P3-016 — Implement context budget allocator

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement context budget allocator.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-017"></a>
## P3-017 — Implement context packer with protected categories

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement context packer with protected categories.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-018"></a>
## P3-018 — Implement Context Scout agent contract

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement Context Scout agent contract.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-019"></a>
## P3-019 — Implement semantic-first scout retrieval flow

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement semantic-first scout retrieval flow.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-020"></a>
## P3-020 — Implement exhaustive reference enumeration mode

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement exhaustive reference enumeration mode.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-021"></a>
## P3-021 — Implement broaden-on-zero-hit policy

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement broaden-on-zero-hit policy.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-022"></a>
## P3-022 — Implement checked negative findings

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement checked negative findings.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-023"></a>
## P3-023 — Implement open-question handoff

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement open-question handoff.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-024"></a>
## P3-024 — Implement read tool line/byte/per-line/token ceilings

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement read tool line/byte/per-line/token ceilings.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-025"></a>
## P3-025 — Implement structured continuation cursors

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement structured continuation cursors.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-026"></a>
## P3-026 — Implement ReadObservation ledger

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement ReadObservation ledger.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-027"></a>
## P3-027 — Implement context visibility generations

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement context visibility generations.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-028"></a>
## P3-028 — Implement context-aware dedup

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement context-aware dedup.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-029"></a>
## P3-029 — Implement post-write context invalidation

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement post-write context invalidation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-030"></a>
## P3-030 — Implement deterministic compaction fallback

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement deterministic compaction fallback.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-031"></a>
## P3-031 — Implement model-assisted structured compaction

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement model-assisted structured compaction.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-032"></a>
## P3-032 — Implement /context explainability metrics

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Implement /context explainability metrics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P2-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p3-gate"></a>
## P3-GATE — Pass Context Fabric, Code Intelligence & Knowledge phase gate

**Phase:** P3 — Context Fabric, Code Intelligence & Knowledge

**Objective:** Pass Context Fabric, Code Intelligence & Knowledge phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/context-engineering.md`
- `docs/api-contracts/context-api.md`

**Dependencies:** P3-001, P3-002, P3-003, P3-004, P3-005, P3-006, P3-007, P3-008, P3-009, P3-010, P3-011, P3-012, P3-013, P3-014, P3-015, P3-016, P3-017, P3-018, P3-019, P3-020, P3-021, P3-022, P3-023, P3-024, P3-025, P3-026, P3-027, P3-028, P3-029, P3-030, P3-031, P3-032

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-001"></a>
## P4-001 — Define WorkspaceView/WorkspaceTransaction contracts

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Define WorkspaceView/WorkspaceTransaction contracts.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-002"></a>
## P4-002 — Implement direct workspace backend

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement direct workspace backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE, P4-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-003"></a>
## P4-003 — Implement git-worktree backend

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement git-worktree backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE, P4-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-004"></a>
## P4-004 — Implement overlay/sandbox backend

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement overlay/sandbox backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE, P4-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-005"></a>
## P4-005 — Implement remote snapshot backend

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement remote snapshot backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-006"></a>
## P4-006 — Implement preimage/hash guarded patch apply

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement preimage/hash guarded patch apply.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-007"></a>
## P4-007 — Implement semantic patch metadata/intent

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement semantic patch metadata/intent.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-008"></a>
## P4-008 — Implement external mutation detector

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement external mutation detector.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-009"></a>
## P4-009 — Implement symbol-level change attribution

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement symbol-level change attribution.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-010"></a>
## P4-010 — Implement child-view transactional integration

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement child-view transactional integration.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-011"></a>
## P4-011 — Implement conflict detection/staging rollback

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement conflict detection/staging rollback.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-012"></a>
## P4-012 — Implement read-before-write freshness invariant

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement read-before-write freshness invariant.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-013"></a>
## P4-013 — Define stable model-visible Tool Gateway

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Define stable model-visible Tool Gateway.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-014"></a>
## P4-014 — Define versioned tool schema registry

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Define versioned tool schema registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-015"></a>
## P4-015 — Implement validate-original-first tool parsing

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement validate-original-first tool parsing.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-016"></a>
## P4-016 — Implement validator issue-path extraction

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement validator issue-path extraction.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-017"></a>
## P4-017 — Implement optional-null repair rule

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement optional-null repair rule.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-018"></a>
## P4-018 — Implement stringified-array/object repair rule

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement stringified-array/object repair rule.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-019"></a>
## P4-019 — Implement singleton-array repair rule

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement singleton-array repair rule.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-020"></a>
## P4-020 — Implement numeric/boolean/enum safe repair rules

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement numeric/boolean/enum safe repair rules.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-021"></a>
## P4-021 — Implement semantic-ambiguity repair refusal

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement semantic-ambiguity repair refusal.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-022"></a>
## P4-022 — Implement repair telemetry/model feedback

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement repair telemetry/model feedback.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-023"></a>
## P4-023 — Implement universal ToolOutcome recovery contract

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement universal ToolOutcome recovery contract.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-024"></a>
## P4-024 — Implement cross-tool invariant engine

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement cross-tool invariant engine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-025"></a>
## P4-025 — Implement Capability vocabulary/normalization

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement Capability vocabulary/normalization.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-026"></a>
## P4-026 — Implement policy precedence engine

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement policy precedence engine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-027"></a>
## P4-027 — Implement CapabilityLease issuance/signature

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement CapabilityLease issuance/signature.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-028"></a>
## P4-028 — Implement executor-side lease verification

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement executor-side lease verification.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-029"></a>
## P4-029 — Implement durable approval queue

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement durable approval queue.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-030"></a>
## P4-030 — Implement dont-ask automation semantics

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement dont-ask automation semantics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-031"></a>
## P4-031 — Implement Capability Projection Engine

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement Capability Projection Engine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-032"></a>
## P4-032 — Implement SecretHandle store

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement SecretHandle store.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-033"></a>
## P4-033 — Implement Credential Broker ephemeral credentials

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement Credential Broker ephemeral credentials.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-034"></a>
## P4-034 — Implement environment-identity-safe credential cache

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Implement environment-identity-safe credential cache.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P3-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p4-gate"></a>
## P4-GATE — Pass Workspace, Tool Gateway, Repair, Policy & Secrets phase gate

**Phase:** P4 — Workspace, Tool Gateway, Repair, Policy & Secrets

**Objective:** Pass Workspace, Tool Gateway, Repair, Policy & Secrets phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/workspace-vcs-semantic-patch.md`
- `docs/architecture/tool-contract-and-recovery.md`
- `docs/architecture/capability-broker-policy-projection.md`

**Dependencies:** P4-001, P4-002, P4-003, P4-004, P4-005, P4-006, P4-007, P4-008, P4-009, P4-010, P4-011, P4-012, P4-013, P4-014, P4-015, P4-016, P4-017, P4-018, P4-019, P4-020, P4-021, P4-022, P4-023, P4-024, P4-025, P4-026, P4-027, P4-028, P4-029, P4-030, P4-031, P4-032, P4-033, P4-034

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-001"></a>
## P5-001 — Define AgentExecutionContext and AgentResult

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Define AgentExecutionContext and AgentResult.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-002"></a>
## P5-002 — Implement node AgentExecutor interface

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement node AgentExecutor interface.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE, P5-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-003"></a>
## P5-003 — Implement role registry and capability profiles

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement role registry and capability profiles.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE, P5-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-004"></a>
## P5-004 — Implement main agent role

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement main agent role.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE, P5-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-005"></a>
## P5-005 — Implement Context Scout role

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement Context Scout role.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-006"></a>
## P5-006 — Implement planner/architect role

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement planner/architect role.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-007"></a>
## P5-007 — Implement coder/debugger roles

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement coder/debugger roles.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-008"></a>
## P5-008 — Implement reviewer/tester roles

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement reviewer/tester roles.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-009"></a>
## P5-009 — Implement independent verifier role

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement independent verifier role.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-010"></a>
## P5-010 — Implement security/performance reviewer roles

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement security/performance reviewer roles.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-011"></a>
## P5-011 — Implement browser/computer operator role

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement browser/computer operator role.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-012"></a>
## P5-012 — Implement release-manager role

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement release-manager role.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-013"></a>
## P5-013 — Implement clean-context TaskEnvelope

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement clean-context TaskEnvelope.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-014"></a>
## P5-014 — Implement typed agent mailbox/result refs

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement typed agent mailbox/result refs.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-015"></a>
## P5-015 — Implement isolated subagent lifecycle

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement isolated subagent lifecycle.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-016"></a>
## P5-016 — Implement persistent read-only specialist lifecycle

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement persistent read-only specialist lifecycle.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-017"></a>
## P5-017 — Implement background specialist bounded summaries

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement background specialist bounded summaries.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-018"></a>
## P5-018 — Implement delegation utility scoring

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement delegation utility scoring.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-019"></a>
## P5-019 — Implement nested delegation depth/budget bounds

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement nested delegation depth/budget bounds.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-020"></a>
## P5-020 — Implement agent cancellation/timeouts

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement agent cancellation/timeouts.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-021"></a>
## P5-021 — Implement empty-response retry guard

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement empty-response retry guard.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-022"></a>
## P5-022 — Implement repeated tool-call loop detector

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement repeated tool-call loop detector.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-023"></a>
## P5-023 — Implement repeated-message/stream loop detector

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement repeated-message/stream loop detector.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-024"></a>
## P5-024 — Implement compact-before-context-overflow retry

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement compact-before-context-overflow retry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-025"></a>
## P5-025 — Implement per-subtask model routing

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement per-subtask model routing.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-026"></a>
## P5-026 — Implement provider/model capability profiles

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement provider/model capability profiles.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-027"></a>
## P5-027 — Implement routing fallback/circuit breakers

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement routing fallback/circuit breakers.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-028"></a>
## P5-028 — Implement PromptRegistry version/hash model

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement PromptRegistry version/hash model.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-029"></a>
## P5-029 — Implement layered PromptComposer

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement layered PromptComposer.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-030"></a>
## P5-030 — Implement AGENTS hierarchy loader

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement AGENTS hierarchy loader.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-031"></a>
## P5-031 — Implement skill metadata registry

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement skill metadata registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-032"></a>
## P5-032 — Implement skill progressive disclosure

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Implement skill progressive disclosure.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P4-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p5-gate"></a>
## P5-GATE — Pass Agent Harness, Prompts, Models & Skills phase gate

**Phase:** P5 — Agent Harness, Prompts, Models & Skills

**Objective:** Pass Agent Harness, Prompts, Models & Skills phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/agent-harness.md`
- `docs/architecture/prompt-runtime.md`
- `system-prompts/system-prompt-composition.md`

**Dependencies:** P5-001, P5-002, P5-003, P5-004, P5-005, P5-006, P5-007, P5-008, P5-009, P5-010, P5-011, P5-012, P5-013, P5-014, P5-015, P5-016, P5-017, P5-018, P5-019, P5-020, P5-021, P5-022, P5-023, P5-024, P5-025, P5-026, P5-027, P5-028, P5-029, P5-030, P5-031, P5-032

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-001"></a>
## P6-001 — Define GoalContract and lifecycle

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Define GoalContract and lifecycle.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-002"></a>
## P6-002 — Implement active/paused/blocked goal state

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement active/paused/blocked goal state.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE, P6-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-003"></a>
## P6-003 — Implement goal create/replace/clear rules

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement goal create/replace/clear rules.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE, P6-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-004"></a>
## P6-004 — Implement goal turn-boundary context injection

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement goal turn-boundary context injection.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE, P6-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-005"></a>
## P6-005 — Implement explicit turn/token/wall-clock budgets

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement explicit turn/token/wall-clock budgets.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-006"></a>
## P6-006 — Implement budget convergence guidance

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement budget convergence guidance.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-007"></a>
## P6-007 — Implement goal crash recovery behavior

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement goal crash recovery behavior.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-008"></a>
## P6-008 — Define Criterion/Claim/Evidence schemas

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Define Criterion/Claim/Evidence schemas.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-009"></a>
## P6-009 — Implement evidence provenance/freshness

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement evidence provenance/freshness.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-010"></a>
## P6-010 — Implement claim support/contradiction edges

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement claim support/contradiction edges.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-011"></a>
## P6-011 — Implement evidence invalidation propagation

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement evidence invalidation propagation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-012"></a>
## P6-012 — Define VerificationRecord/verdict

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Define VerificationRecord/verdict.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-013"></a>
## P6-013 — Implement deterministic verifier adapters

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement deterministic verifier adapters.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-014"></a>
## P6-014 — Implement independent verifier Agent node

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement independent verifier Agent node.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-015"></a>
## P6-015 — Implement verifier context isolation

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement verifier context isolation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-016"></a>
## P6-016 — Implement CompletionCandidate contract

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement CompletionCandidate contract.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-017"></a>
## P6-017 — Implement host GoalCompletionGate

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement host GoalCompletionGate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-018"></a>
## P6-018 — Implement false-completion rejection path

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement false-completion rejection path.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-019"></a>
## P6-019 — Implement rejection gap→repair graph expansion

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement rejection gap→repair graph expansion.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-020"></a>
## P6-020 — Implement visual vs deterministic proof precedence

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement visual vs deterministic proof precedence.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-021"></a>
## P6-021 — Implement goal pause/resume/cancel kernel APIs

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement goal pause/resume/cancel kernel APIs.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-022"></a>
## P6-022 — Implement goal/evidence TUI inspector

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement goal/evidence TUI inspector.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-023"></a>
## P6-023 — Implement goal export/attestation

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Implement goal export/attestation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-024"></a>
## P6-024 — Add adversarial false-completion eval suite

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Add adversarial false-completion eval suite.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P5-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p6-gate"></a>
## P6-GATE — Pass Goals, Evidence & Independent Verification phase gate

**Phase:** P6 — Goals, Evidence & Independent Verification

**Objective:** Pass Goals, Evidence & Independent Verification phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/goals-evidence-verification.md`
- `docs/api-contracts/goal-evidence-api.md`

**Dependencies:** P6-001, P6-002, P6-003, P6-004, P6-005, P6-006, P6-007, P6-008, P6-009, P6-010, P6-011, P6-012, P6-013, P6-014, P6-015, P6-016, P6-017, P6-018, P6-019, P6-020, P6-021, P6-022, P6-023, P6-024

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-001"></a>
## P7-001 — Define ProcessSpec/ProcessHandle/ProcessOutcome

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Define ProcessSpec/ProcessHandle/ProcessOutcome.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-002"></a>
## P7-002 — Implement process group/tree ownership

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement process group/tree ownership.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE, P7-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-003"></a>
## P7-003 — Implement PTY sessions and input generation guards

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement PTY sessions and input generation guards.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE, P7-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-004"></a>
## P7-004 — Implement bounded stdout/stderr head-tail results

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement bounded stdout/stderr head-tail results.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE, P7-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-005"></a>
## P7-005 — Implement full process log Artifact spooling

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement full process log Artifact spooling.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-006"></a>
## P7-006 — Implement foreground→daemon detach policy

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement foreground→daemon detach policy.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-007"></a>
## P7-007 — Implement process timeout/cancellation

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement process timeout/cancellation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-008"></a>
## P7-008 — Implement protected-user-process safeguards

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement protected-user-process safeguards.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-009"></a>
## P7-009 — Define MonitorSpec and event predicates

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Define MonitorSpec and event predicates.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-010"></a>
## P7-010 — Implement exit-code monitor

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement exit-code monitor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-011"></a>
## P7-011 — Implement regex/log-event monitor

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement regex/log-event monitor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-012"></a>
## P7-012 — Implement port/readiness monitor

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement port/readiness monitor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-013"></a>
## P7-013 — Implement file/event monitor

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement file/event monitor.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-014"></a>
## P7-014 — Implement graph wake-on-monitor events

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement graph wake-on-monitor events.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-015"></a>
## P7-015 — Define TriggerSpec and durable cursors

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Define TriggerSpec and durable cursors.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-016"></a>
## P7-016 — Implement cron trigger parser/scheduler

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement cron trigger parser/scheduler.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-017"></a>
## P7-017 — Implement deterministic per-job jitter

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement deterministic per-job jitter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-018"></a>
## P7-018 — Implement JobLease generation/heartbeat

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement JobLease generation/heartbeat.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-019"></a>
## P7-019 — Implement missed one-shot semantics

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement missed one-shot semantics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-020"></a>
## P7-020 — Define SandboxSpec/backend trait

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Define SandboxSpec/backend trait.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-021"></a>
## P7-021 — Implement host-restricted backend

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement host-restricted backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-022"></a>
## P7-022 — Implement rootless/container backend

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement rootless/container backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-023"></a>
## P7-023 — Implement stronger kernel/microVM adapter seam

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement stronger kernel/microVM adapter seam.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-024"></a>
## P7-024 — Define ResourcePool/EnvironmentLease

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Define ResourcePool/EnvironmentLease.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-025"></a>
## P7-025 — Implement warm acquire/release

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement warm acquire/release.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-026"></a>
## P7-026 — Implement environment phase telemetry

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement environment phase telemetry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-027"></a>
## P7-027 — Implement pool sanitation/quarantine

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement pool sanitation/quarantine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-028"></a>
## P7-028 — Implement pool miss fallback provisioning

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement pool miss fallback provisioning.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-029"></a>
## P7-029 — Implement remote worker resource backend

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Implement remote worker resource backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-030"></a>
## P7-030 — Add disconnect/install-interruption chaos tests

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Add disconnect/install-interruption chaos tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P6-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p7-gate"></a>
## P7-GATE — Pass Process, Sandbox, Resource Pool, Jobs & Triggers phase gate

**Phase:** P7 — Process, Sandbox, Resource Pool, Jobs & Triggers

**Objective:** Pass Process, Sandbox, Resource Pool, Jobs & Triggers phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/process-background-trigger-runtime.md`
- `docs/architecture/sandbox-resource-pool.md`

**Dependencies:** P7-001, P7-002, P7-003, P7-004, P7-005, P7-006, P7-007, P7-008, P7-009, P7-010, P7-011, P7-012, P7-013, P7-014, P7-015, P7-016, P7-017, P7-018, P7-019, P7-020, P7-021, P7-022, P7-023, P7-024, P7-025, P7-026, P7-027, P7-028, P7-029, P7-030

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-001"></a>
## P8-001 — Define SurfaceRef/Observation/generation contracts

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Define SurfaceRef/Observation/generation contracts.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-002"></a>
## P8-002 — Define ComputerAction schema

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Define ComputerAction schema.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE, P8-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-003"></a>
## P8-003 — Implement coordinate normalization/scaling

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement coordinate normalization/scaling.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE, P8-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-004"></a>
## P8-004 — Implement pointer state/move/click/down/up

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement pointer state/move/click/down/up.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE, P8-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-005"></a>
## P8-005 — Implement double-click and drag

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement double-click and drag.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-006"></a>
## P8-006 — Implement keyboard key/chord/type

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement keyboard key/chord/type.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-007"></a>
## P8-007 — Implement scroll and wait conditions

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement scroll and wait conditions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-008"></a>
## P8-008 — Implement screenshot/region/cursor actions

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement screenshot/region/cursor actions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-009"></a>
## P8-009 — Implement action batching rules

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement action batching rules.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-010"></a>
## P8-010 — Implement settle/reobserve policy

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement settle/reobserve policy.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-011"></a>
## P8-011 — Implement screenshot-on-failure diagnostics

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement screenshot-on-failure diagnostics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-012"></a>
## P8-012 — Implement action metrics/trace events

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement action metrics/trace events.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-013"></a>
## P8-013 — Implement BrowserSession lifecycle

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement BrowserSession lifecycle.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-014"></a>
## P8-014 — Implement DOM/test-id locator actions

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement DOM/test-id locator actions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-015"></a>
## P8-015 — Implement accessibility-tree browser actions

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement accessibility-tree browser actions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-016"></a>
## P8-016 — Implement CDP allowlist bridge

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement CDP allowlist bridge.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-017"></a>
## P8-017 — Implement browser console/network capture

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement browser console/network capture.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-018"></a>
## P8-018 — Implement browser JS execution policy gate

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement browser JS execution policy gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-019"></a>
## P8-019 — Implement download/upload policy paths

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement download/upload policy paths.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-020"></a>
## P8-020 — Implement prompt-injection fencing for page content

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement prompt-injection fencing for page content.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-021"></a>
## P8-021 — Implement macOS AX backend

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement macOS AX backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-022"></a>
## P8-022 — Implement Windows UIA backend

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement Windows UIA backend.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-023"></a>
## P8-023 — Implement Linux AT-SPI/platform pointer seam

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement Linux AT-SPI/platform pointer seam.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-024"></a>
## P8-024 — Implement PreviewSupervisor lifecycle

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement PreviewSupervisor lifecycle.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-025"></a>
## P8-025 — Implement dynamic port/readiness checks

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement dynamic port/readiness checks.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-026"></a>
## P8-026 — Implement compiler/HMR/HTTP diagnostics

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement compiler/HMR/HTTP diagnostics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-027"></a>
## P8-027 — Implement preview console/network feedback

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement preview console/network feedback.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-028"></a>
## P8-028 — Implement visual evidence recorder

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement visual evidence recorder.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-029"></a>
## P8-029 — Implement Android emulator/ADB adapter

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement Android emulator/ADB adapter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-030"></a>
## P8-030 — Implement iOS simctl remote-mac adapter

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement iOS simctl remote-mac adapter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-031"></a>
## P8-031 — Implement ControlLease human takeover

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement ControlLease human takeover.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-032"></a>
## P8-032 — Implement post-takeover mutation/surface reconciliation

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Implement post-takeover mutation/surface reconciliation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P7-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p8-gate"></a>
## P8-GATE — Pass Browser, Computer Use, Mobile & Preview phase gate

**Phase:** P8 — Browser, Computer Use, Mobile & Preview

**Objective:** Pass Browser, Computer Use, Mobile & Preview phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/computer-use.md`
- `docs/architecture/preview-visual-verification.md`
- `docs/reference/computer-use-command-reference.md`

**Dependencies:** P8-001, P8-002, P8-003, P8-004, P8-005, P8-006, P8-007, P8-008, P8-009, P8-010, P8-011, P8-012, P8-013, P8-014, P8-015, P8-016, P8-017, P8-018, P8-019, P8-020, P8-021, P8-022, P8-023, P8-024, P8-025, P8-026, P8-027, P8-028, P8-029, P8-030, P8-031, P8-032

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-001"></a>
## P9-001 — Define MCP catalog revision model

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Define MCP catalog revision model.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-002"></a>
## P9-002 — Implement MCP client discovery/cache

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement MCP client discovery/cache.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE, P9-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-003"></a>
## P9-003 — Implement MCP invocation middleware chain

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement MCP invocation middleware chain.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE, P9-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-004"></a>
## P9-004 — Implement MCP output trust fencing

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement MCP output trust fencing.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE, P9-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-005"></a>
## P9-005 — Implement explicit RapidLM MCP server surface

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement explicit RapidLM MCP server surface.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-006"></a>
## P9-006 — Implement ACP stdio framing

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement ACP stdio framing.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-007"></a>
## P9-007 — Implement ACP session/progress/diff/permission mapping

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement ACP session/progress/diff/permission mapping.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-008"></a>
## P9-008 — Implement versioned headless JSONL schema

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement versioned headless JSONL schema.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-009"></a>
## P9-009 — Implement JSONL stdout purity tests

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement JSONL stdout purity tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-010"></a>
## P9-010 — Implement TypeScript SDK generated contracts

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement TypeScript SDK generated contracts.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-011"></a>
## P9-011 — Implement SDK reconnect/event cursors

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement SDK reconnect/event cursors.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-012"></a>
## P9-012 — Define lifecycle HookEvent registry

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Define lifecycle HookEvent registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-013"></a>
## P9-013 — Implement hook filter/matcher engine

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement hook filter/matcher engine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-014"></a>
## P9-014 — Implement hook timeouts/process-group cleanup

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement hook timeouts/process-group cleanup.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-015"></a>
## P9-015 — Implement per-event fail-open/fail-closed semantics

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement per-event fail-open/fail-closed semantics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-016"></a>
## P9-016 — Implement hook capability restrictions

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement hook capability restrictions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-017"></a>
## P9-017 — Define WASM plugin manifest/WIT host API

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Define WASM plugin manifest/WIT host API.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-018"></a>
## P9-018 — Implement WASM capability intersection

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement WASM capability intersection.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-019"></a>
## P9-019 — Implement plugin lifecycle/update/disable

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement plugin lifecycle/update/disable.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-020"></a>
## P9-020 — Implement skill/plugin marketplace metadata seam

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement skill/plugin marketplace metadata seam.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-021"></a>
## P9-021 — Implement Playbook→initial graph compiler

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement Playbook→initial graph compiler.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-022"></a>
## P9-022 — Implement external ACP agent adapter

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement external ACP agent adapter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-023"></a>
## P9-023 — Implement external CLI agent adapter

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement external CLI agent adapter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-024"></a>
## P9-024 — Implement external agent result/evidence normalization

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement external agent result/evidence normalization.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-025"></a>
## P9-025 — Define HandoffBundle

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Define HandoffBundle.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-026"></a>
## P9-026 — Implement SessionExecutionLease generation transfer

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement SessionExecutionLease generation transfer.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-027"></a>
## P9-027 — Implement local↔remote handoff state machine

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement local↔remote handoff state machine.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-028"></a>
## P9-028 — Implement fresh target lease/credential/observation issuance

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Implement fresh target lease/credential/observation issuance.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P8-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p9-gate"></a>
## P9-GATE — Pass MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff phase gate

**Phase:** P9 — MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff

**Objective:** Pass MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/mcp-acp-sdk-external-agents.md`
- `docs/architecture/plugins-skills-hooks.md`
- `docs/architecture/remote-workers-handoff-control.md`

**Dependencies:** P9-001, P9-002, P9-003, P9-004, P9-005, P9-006, P9-007, P9-008, P9-009, P9-010, P9-011, P9-012, P9-013, P9-014, P9-015, P9-016, P9-017, P9-018, P9-019, P9-020, P9-021, P9-022, P9-023, P9-024, P9-025, P9-026, P9-027, P9-028

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-001"></a>
## P10-001 — Implement rapid composition root/startup

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement rapid composition root/startup.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-002"></a>
## P10-002 — Implement daemon IPC/authentication

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement daemon IPC/authentication.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE, P10-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-003"></a>
## P10-003 — Implement KernelClient in-process/IPC transports

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement KernelClient in-process/IPC transports.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE, P10-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-004"></a>
## P10-004 — Implement interactive prompt/transcript reducer

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement interactive prompt/transcript reducer.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE, P10-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-005"></a>
## P10-005 — Implement streaming coalescing/backpressure

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement streaming coalescing/backpressure.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-006"></a>
## P10-006 — Implement 80-column/NO_COLOR rendering

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement 80-column/NO_COLOR rendering.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-007"></a>
## P10-007 — Implement command palette/slash command registry

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement command palette/slash command registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-008"></a>
## P10-008 — Implement graph inspector UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement graph inspector UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-009"></a>
## P10-009 — Implement context inspector UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement context inspector UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-010"></a>
## P10-010 — Implement agent inspector UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement agent inspector UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-011"></a>
## P10-011 — Implement files/diff WorkspaceTransaction UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement files/diff WorkspaceTransaction UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-012"></a>
## P10-012 — Implement terminal/process monitor UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement terminal/process monitor UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-013"></a>
## P10-013 — Implement approvals UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement approvals UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-014"></a>
## P10-014 — Implement evidence/verification UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement evidence/verification UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-015"></a>
## P10-015 — Implement browser/computer panel

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement browser/computer panel.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-016"></a>
## P10-016 — Implement resources/sandbox panel

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement resources/sandbox panel.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-017"></a>
## P10-017 — Implement memory/knowledge/preference inspector

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement memory/knowledge/preference inspector.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-018"></a>
## P10-018 — Implement trace/events inspector

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement trace/events inspector.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-019"></a>
## P10-019 — Implement model/router selector/status

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement model/router selector/status.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-020"></a>
## P10-020 — Implement goal card/budget controls

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement goal card/budget controls.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-021"></a>
## P10-021 — Implement human-control ownership indicator

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement human-control ownership indicator.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-022"></a>
## P10-022 — Implement session list/search/resume

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement session list/search/resume.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-023"></a>
## P10-023 — Implement timeline/checkpoint UI

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement timeline/checkpoint UI.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-024"></a>
## P10-024 — Implement conversation/graph/workspace rewind

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement conversation/graph/workspace rewind.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-025"></a>
## P10-025 — Implement non-destructive fork from checkpoint

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement non-destructive fork from checkpoint.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-026"></a>
## P10-026 — Implement headless rapid exec/run

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement headless rapid exec/run.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-027"></a>
## P10-027 — Implement rapid inspect/export

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement rapid inspect/export.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-028"></a>
## P10-028 — Implement rapid doctor diagnostics

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement rapid doctor diagnostics.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-029"></a>
## P10-029 — Implement shell completion/man/help generation

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Implement shell completion/man/help generation.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-030"></a>
## P10-030 — Benchmark startup/10k-event replay/render

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Benchmark startup/10k-event replay/render.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P9-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p10-gate"></a>
## P10-GATE — Pass CLI, TUI, Daemon, Inspectors & Time Travel phase gate

**Phase:** P10 — CLI, TUI, Daemon, Inspectors & Time Travel

**Objective:** Pass CLI, TUI, Daemon, Inspectors & Time Travel phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/cli-tui.md`
- `docs/architecture/sessions-checkpoints-time-travel.md`

**Dependencies:** P10-001, P10-002, P10-003, P10-004, P10-005, P10-006, P10-007, P10-008, P10-009, P10-010, P10-011, P10-012, P10-013, P10-014, P10-015, P10-016, P10-017, P10-018, P10-019, P10-020, P10-021, P10-022, P10-023, P10-024, P10-025, P10-026, P10-027, P10-028, P10-029, P10-030

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-001"></a>
## P11-001 — Define eval Scenario/Fixture schemas

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Define eval Scenario/Fixture schemas.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-002"></a>
## P11-002 — Implement FixtureManager CAS repos/services

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement FixtureManager CAS repos/services.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE, P11-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-003"></a>
## P11-003 — Implement ScriptedModel

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement ScriptedModel.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE, P11-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-004"></a>
## P11-004 — Implement ReplayProvider

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement ReplayProvider.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE, P11-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-005"></a>
## P11-005 — Implement LiveProvider experiment pinning

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement LiveProvider experiment pinning.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-006"></a>
## P11-006 — Implement production KernelRunner modes

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement production KernelRunner modes.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-007"></a>
## P11-007 — Implement FaultInjector framework

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement FaultInjector framework.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-008"></a>
## P11-008 — Implement graph assertions

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement graph assertions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-009"></a>
## P11-009 — Implement file/workspace assertions

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement file/workspace assertions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-010"></a>
## P11-010 — Implement policy/capability assertions

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement policy/capability assertions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-011"></a>
## P11-011 — Implement process/resource assertions

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement process/resource assertions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-012"></a>
## P11-012 — Implement context/evidence assertions

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement context/evidence assertions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-013"></a>
## P11-013 — Implement browser/computer assertions

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement browser/computer assertions.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-014"></a>
## P11-014 — Implement deterministic grader registry

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement deterministic grader registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-015"></a>
## P11-015 — Implement optional rubric JudgeAdapter

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement optional rubric JudgeAdapter.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-016"></a>
## P11-016 — Implement MetricCollector registry

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement MetricCollector registry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-017"></a>
## P11-017 — Implement FailureBundler

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement FailureBundler.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-018"></a>
## P11-018 — Implement TrajectoryCollector

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement TrajectoryCollector.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-019"></a>
## P11-019 — Implement trajectory privacy classification

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement trajectory privacy classification.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-020"></a>
## P11-020 — Implement CandidateRanker

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement CandidateRanker.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-021"></a>
## P11-021 — Implement ExperimentRegistry

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement ExperimentRegistry.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-022"></a>
## P11-022 — Define FeedbackEvent accept/reject/edit

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Define FeedbackEvent accept/reject/edit.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-023"></a>
## P11-023 — Implement PreferenceCandidate extraction

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement PreferenceCandidate extraction.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-024"></a>
## P11-024 — Implement preference confidence/scope/decay

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement preference confidence/scope/decay.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-025"></a>
## P11-025 — Implement preference conflict handling

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement preference conflict handling.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-026"></a>
## P11-026 — Implement Session Insights analyzers

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement Session Insights analyzers.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-027"></a>
## P11-027 — Implement held-out promotion gates

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement held-out promotion gates.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-028"></a>
## P11-028 — Implement 1h/4h/12h/24h endurance harness

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement 1h/4h/12h/24h endurance harness.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-029"></a>
## P11-029 — Implement >=1000-tool-call release scenario

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement >=1000-tool-call release scenario.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-030"></a>
## P11-030 — Implement verified-success-per-token benchmark

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Implement verified-success-per-token benchmark.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P10-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p11-gate"></a>
## P11-GATE — Pass Eval Harness, Trajectories, Preferences & Optimization phase gate

**Phase:** P11 — Eval Harness, Trajectories, Preferences & Optimization

**Objective:** Pass Eval Harness, Trajectories, Preferences & Optimization phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/eval-harness.md`
- `docs/architecture/preference-learning-trajectory.md`

**Dependencies:** P11-001, P11-002, P11-003, P11-004, P11-005, P11-006, P11-007, P11-008, P11-009, P11-010, P11-011, P11-012, P11-013, P11-014, P11-015, P11-016, P11-017, P11-018, P11-019, P11-020, P11-021, P11-022, P11-023, P11-024, P11-025, P11-026, P11-027, P11-028, P11-029, P11-030

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-001"></a>
## P12-001 — Create V3 STRIDE/data-flow threat model

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Create V3 STRIDE/data-flow threat model.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-002"></a>
## P12-002 — Implement project trust gating

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement project trust gating.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE, P12-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-003"></a>
## P12-003 — Implement symlink/realpath path escape tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement symlink/realpath path escape tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE, P12-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-004"></a>
## P12-004 — Implement prompt-injection trust tagging

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement prompt-injection trust tagging.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE, P12-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-005"></a>
## P12-005 — Implement MCP/plugin/hook privilege-escalation tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement MCP/plugin/hook privilege-escalation tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-006"></a>
## P12-006 — Implement secret redaction/exfiltration tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement secret redaction/exfiltration tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-007"></a>
## P12-007 — Implement egress policy/receipt tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement egress policy/receipt tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-008"></a>
## P12-008 — Implement approval bypass negative tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement approval bypass negative tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-009"></a>
## P12-009 — Implement dont-ask no-hang tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement dont-ask no-hang tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-010"></a>
## P12-010 — Implement stale CapabilityLease tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement stale CapabilityLease tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-011"></a>
## P12-011 — Implement stale Observation coordinate tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement stale Observation coordinate tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-012"></a>
## P12-012 — Implement stale ReadObservation write tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement stale ReadObservation write tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-013"></a>
## P12-013 — Implement uncertain-effect double-send tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement uncertain-effect double-send tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-014"></a>
## P12-014 — Implement sandbox downgrade refusal tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement sandbox downgrade refusal tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-015"></a>
## P12-015 — Implement resource pool cross-tenant contamination tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement resource pool cross-tenant contamination tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-016"></a>
## P12-016 — Implement credential cache identity isolation tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement credential cache identity isolation tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-017"></a>
## P12-017 — Implement remote worker forged-result tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement remote worker forged-result tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-018"></a>
## P12-018 — Implement handoff split-brain partition tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement handoff split-brain partition tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-019"></a>
## P12-019 — Implement plugin WASM resource exhaustion tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement plugin WASM resource exhaustion tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-020"></a>
## P12-020 — Implement process tree escape/cancellation tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement process tree escape/cancellation tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-021"></a>
## P12-021 — Implement archive/path traversal tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement archive/path traversal tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-022"></a>
## P12-022 — Implement dependency/lockfile integrity scans

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement dependency/lockfile integrity scans.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-023"></a>
## P12-023 — Implement secret/dependency/SAST scanner nodes

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement secret/dependency/SAST scanner nodes.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-024"></a>
## P12-024 — Implement fuzzers for graph/event/tool decoders

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement fuzzers for graph/event/tool decoders.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-025"></a>
## P12-025 — Implement persistence corruption recovery tests

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Implement persistence corruption recovery tests.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-026"></a>
## P12-026 — Run full adversarial security release gate

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Run full adversarial security release gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P11-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p12-gate"></a>
## P12-GATE — Pass Security, Reliability & Adversarial Hardening phase gate

**Phase:** P12 — Security, Reliability & Adversarial Hardening

**Objective:** Pass Security, Reliability & Adversarial Hardening phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/security/threat-model.md`
- `docs/architecture/security-scanners.md`

**Dependencies:** P12-001, P12-002, P12-003, P12-004, P12-005, P12-006, P12-007, P12-008, P12-009, P12-010, P12-011, P12-012, P12-013, P12-014, P12-015, P12-016, P12-017, P12-018, P12-019, P12-020, P12-021, P12-022, P12-023, P12-024, P12-025, P12-026

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-001"></a>
## P13-001 — Audit/lock Rust toolchain and Cargo workspace

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Audit/lock Rust toolchain and Cargo workspace.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-002"></a>
## P13-002 — Audit/lock Node/TypeScript SDK toolchain

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Audit/lock Node/TypeScript SDK toolchain.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE, P13-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-003"></a>
## P13-003 — Implement targeted CI crate/package matrix

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement targeted CI crate/package matrix.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE, P13-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-004"></a>
## P13-004 — Implement format/lint/static analysis gates

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement format/lint/static analysis gates.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE, P13-001

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-005"></a>
## P13-005 — Implement unit/integration/e2e test matrix

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement unit/integration/e2e test matrix.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-006"></a>
## P13-006 — Implement deterministic replay CI gate

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement deterministic replay CI gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-007"></a>
## P13-007 — Implement token/performance regression gates

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement token/performance regression gates.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-008"></a>
## P13-008 — Implement cross-platform macOS/Linux/Windows builds

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement cross-platform macOS/Linux/Windows builds.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-009"></a>
## P13-009 — Implement optional remote mac/iOS test lane

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement optional remote mac/iOS test lane.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-010"></a>
## P13-010 — Generate SBOM and dependency/license reports

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Generate SBOM and dependency/license reports.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-011"></a>
## P13-011 — Implement artifact signing/provenance attestations

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement artifact signing/provenance attestations.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-012"></a>
## P13-012 — Implement release manifest/update channels

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement release manifest/update channels.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-013"></a>
## P13-013 — Implement updater signature/digest verification

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement updater signature/digest verification.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-014"></a>
## P13-014 — Implement rollback/failed-update recovery

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement rollback/failed-update recovery.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-015"></a>
## P13-015 — Implement migration of V2 Event/Goal/Workspace data

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement migration of V2 Event/Goal/Workspace data.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-016"></a>
## P13-016 — Implement migration compatibility fixtures

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement migration compatibility fixtures.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-017"></a>
## P13-017 — Implement CLI/API schema compatibility fixtures

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Implement CLI/API schema compatibility fixtures.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-018"></a>
## P13-018 — Write operator incident-recovery runbook

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Write operator incident-recovery runbook.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-019"></a>
## P13-019 — Write observability/SLO runbook

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Write observability/SLO runbook.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-020"></a>
## P13-020 — Write complete user CLI/TUI reference

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Write complete user CLI/TUI reference.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-021"></a>
## P13-021 — Write extension/SDK developer guide

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Write extension/SDK developer guide.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-022"></a>
## P13-022 — Run requirements traceability audit

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Run requirements traceability audit.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-023"></a>
## P13-023 — Run clean-install/upgrade/rollback acceptance

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Run clean-install/upgrade/rollback acceptance.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-024"></a>
## P13-024 — Execute GA release gate and archive evidence

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Execute GA release gate and archive evidence.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P12-GATE

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.

<a id="p13-gate"></a>
## P13-GATE — Pass Build, Release, Documentation & GA Migration phase gate

**Phase:** P13 — Build, Release, Documentation & GA Migration

**Objective:** Pass Build, Release, Documentation & GA Migration phase gate.

**Required execution:**
1. Inspect the current repository and verify the existing ownership boundary; do not implement from dossier assumptions alone.
2. State the concrete task contract, affected modules/schemas, migration impact and named acceptance evidence.
3. Gather only necessary context using exact/structural/semantic methods; for non-trivial discovery use Context Scout coverage/negative-finding rules.
4. If orchestration is needed, express it as the smallest valid Runtime Graph/subgraph; models propose, host contracts remain authoritative.
5. Implement through existing seams; do not add a duplicate service/store/scheduler/policy/context authority.
6. Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries.
7. Verify configured build/lint/test/security/performance gates relevant to this task and capture observed results.
8. Use independent verification for completion-critical claims; invalidate/re-run evidence after relevant writes.
9. Record changed files, contracts, commands/results, evidence refs, residual risks and ledger status; clear transient task context.

**Reference documents:**
- `docs/architecture/build-release-supply-chain.md`
- `docs/operations/build-and-ci.md`
- `docs/operations/release-and-supply-chain.md`

**Dependencies:** P13-001, P13-002, P13-003, P13-004, P13-005, P13-006, P13-007, P13-008, P13-009, P13-010, P13-011, P13-012, P13-013, P13-014, P13-015, P13-016, P13-017, P13-018, P13-019, P13-020, P13-021, P13-022, P13-023, P13-024

**Definition of done:** repository truth matches the intended contract; required tests/evidence are current; no blocking security/data-loss/false-completion issue remains.
