# B. V2 Historical Implementation Evidence

## Recovery method (read-only)

- `git log --all --oneline` → **only 2 commits**: `c28572d` (docs migration, this session) and `051fef1` ("Initial import of RapidLM CLI V2 dossier and workspace", 2026-08-24).
- `git log -S<term>` for `HANDOFF`, `ledger`, `checkpoint`, `migration`, `completed`, `verified`, `acceptance`, `V1`, `V2` → no additional commits; the entire repo was imported as a single snapshot.
- No branches/tags other than `main`/`origin/main`. No historical handoff/implementation-ledger commits exist. History was **not** rewritten or checked out over.

**Conclusion:** there is no incremental V2 implementation history to recover. "Historical V2 evidence" is limited to (a) the single import commit and (b) the archived V2 dossier under `docs/v2-archive/`.

## Evidence artifacts

| Artifact | Revision / date | Claims | Current verification | Reliability |
|---|---|---|---|---|
| `git` commit `051fef1` | 2026-08-24 | "Initial import of RapidLM CLI V2 dossier and workspace" — imported V2 dossier + Rust workspace as one snapshot | Imported wholesale; no per-feature completion metadata, no handoffs | Low for feature status — it is a single snapshot with no incremental claims |
| `docs/v2-archive/validation-report.md` | 2026-08-14 | 320 tasks; 146 PRD reqs; 33 arch docs; 15 API docs; 19 ADRs; doc-internal structural checks all PASS | Checks are **document consistency only** (task IDs match `prompts.md`, links resolve). Contains **no code/runtime/CI verification** | Low — asserts dossier completeness, not implementation |
| `docs/v2-archive/V2-CHANGELOG.md` | 2026-08-14 | V2 extends V1 with: Managed Agent Mesh, Execution Handoff + Human Takeover, Knowledge + Playbooks, Trajectory Learning + Session Insights, Computer Use V2 | The named subsystems map to current crates that are **empty shells**: `agent-pool`, `handoff`, `trajectory`, `knowledge`, `playbooks`, `insights`, `harness`. `computer-use` is partially built (logic real, live drivers stubbed) | Medium for intent, Low for "implemented" — these are precisely the unbuilt crates |
| `docs/v2-archive/requirements-traceability.md` (V2) | 2026-08-14 | Maps V2 requirements → tasks | Doc-internal; references a V2 implementation DAG not present in current source | Low — no corresponding code |
| `docs/development-ledger.md` (current V3 dossier) | 2026-08-24 | Every V3 task P0–P11 marked `NOT_STARTED` | **Contradicted by source**: kernel/event-ledger/context-engine/capability-broker/etc. are substantially implemented + tested | Unreliable as a status record (false-negative) |
| `docs/research/source-ledger.md` | 2026-08-24 | Provenance of research synthesis (Kimi Code, Grok Build, Muse, Devin, Qwen, etc.) | Descriptive, version-specific; explicitly "not claims about current universal product behavior" | N/A — research provenance, not impl status |
| `docs/research/migration-v2-to-v3.md` | 2026-08-24 | All V2 concepts → `KEEP`/`EXTEND`; Phase 0 must emit per-crate `KEEP\|ADAPT\|REPLACE\|DELETE` | Design intent; consistent with preserving V1-foundation crates | Medium — drives disposition in §C |
| Uncommitted working tree vs HEAD `c28572d` | Pre-existed this goal (listed in conversation initial `git_status` before any calibration tool call) | 49 tracked files modified (incl. `.grok/workflows/rapidlm-v2-implement.rhai` `capability_mode: "read-only"` → `"execute"` at verify/security/reverify/phase-gate jobs) + ~200 untracked source files of in-progress V3 implementation | Independently confirmed: this goal never invoked the `workflow` tool, never wrote that `.rhai` file, ran all 8 audit subagents with `capability_mode: "read-only"`. The dirty tree is the user's pre-existing uncommitted V3 implementation; the user previously chose not to commit it. **Not reverted** — reverting would destroy user work, which the goal forbids. | High for provenance of *this goal's* writes (only `calibration/` added); the dirty tree is not a V2 completion claim |

## Reconciliation of V2 claims against current source

1. **The V2 dossier describes a system the current tree is NOT.** The current workspace uses V3 names (`agent-runtime`, `capability-broker`, `context-engine`, `llm-router`, `computer-use`, `mobile-sim`, `mcp`, `acp`, `plugin-host`) and carries forward V1-foundation crates (`kernel`, `event-ledger`, `tool-gateway`, `sandbox`, `process-supervisor`, `workspace`, `vcs`, `auth`, `protocol`, `security`, `telemetry`, `tui`). The "V2 additions" named in `V2-CHANGELOG.md` split into two groups in the current tree:
   - **Built (partial/full):** `computer-use`, `mobile-sim`, `mcp`, `acp`, `plugin-host`.
   - **Empty shells (0 impl):** `agent-pool`, `handoff`, `trajectory`, `knowledge`, `playbooks`, `insights`, `harness`.
2. **No V2 runtime validation exists.** There is no test report, no recorded CI run (`.github/workflows/ci.yml` exists but no run history in git), no acceptance evidence. All "validation" is documentation-internal.
3. **The `NOT_STARTED` ledger is a false baseline.** It must be overwritten by source-derived calibration (this document set) before implementation proceeds.

## Reliability grade

- Historical **implementation-status** evidence: **absent** (single snapshot import, doc-only validation).
- Historical **design-intent** evidence: **present and usable** (`migration-v2-to-v3.md`, `V2-CHANGELOG.md`, V2 architecture docs) for driving V2→V3 dispositions.

**Rule applied:** no V2 historical claim was treated as authoritative for current state; each was independently verified against source. Where source and V2 docs disagree, source wins and the discrepancy is recorded (see §C, §E).
