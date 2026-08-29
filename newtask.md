# newtask.md — Parity-then-Leapfrog Roadmap: RapidLM vs. Grok Build vs. Qwen Code

- **Date:** 2026-08-29
- **RapidLM baseline:** this workspace @ commit `687c745` (verified directly from source, not from `gaps.md`'s
  own tables — several of that document's "current state" rows were found stale during this pass; see
  the note in §0).
- **Competitor baselines:** Grok Build @ `9684fa3` (`gaps.md`, full source-grounded audit, cloned at
  `/Users/mohsin/grokbuild`); Qwen Code @ `265e7f1` (fresh source-grounded audit, this session, cloned
  shallow at `/private/tmp/claude-501/qwen-code-src`).
- **Third input — Modbit:** `/Users/mohsin/useful /Modbit_Feature_Inspiration_Provenance_2026-08-18_v2.md`
  (and the wider Modbit dossier at that path and at `/Users/mohsin/modbit`). Modbit is a separate, more
  mature product (an Electron/Code-OSS IDE fork with its own Rust context engine, policy kernel, change
  engine, verification plane, etc.) that the user has been designing across prior sessions. It is used
  here strictly as a **mechanism reference**: every item pulled from it below is named by its Modbit
  feature ID for traceability, but is to be **independently, natively reimplemented** against RapidLM's
  own crates — never ported as code, never treated as a dependency. This mirrors Modbit's own stated rule
  for its competitor research (`GOV-005`, "clean-room competitor adoption").
- **Severity scale:** `P0` = blocks credible parity · `P1` = closes a real competitive gap · `P2` = polish
  or a leapfrog bet · **Effort:** S / M / L, rough shape not a schedule commitment.

---

## 0a. Meta-finding from this implementation pass (read this before Phase 2 especially)

Nearly every gap this pass actually investigated in depth turned out to be smaller than written, for
the same underlying reason: **this codebase already contains mature, well-tested implementations of
most of the sophisticated primitives Phase 2 asks for — they are simply not wired into the exec loop
`apps/rapid` actually runs.** This is the exact same shape as the context-engine finding from commit
`687c745` (~92% of that crate was built and tested but unreachable from `rapid exec`), and it recurred
repeatedly during this pass:

- `crates/sandbox` — a full tiered `SandboxManager`/`SandboxBackend` system (HostRestricted/Container/
  Gvisor/RemoteWorker, ~11,400 lines) exists; `apps/rapid` doesn't depend on the crate at all (§1.1).
- `crates/capability-broker` — a complete `CapabilityLease`/policy/approval system exists (used
  correctly in test fixtures across several crates); no production code path in the whole repo mints a
  real lease today, confirmed while scoping §1.1's second correction.
- `crates/llm-router` — real per-request cost (`UsageCost::Reported`) and a full `ModelCatalog` with
  pricing/latency/context-limits already exist; the value is computed then dropped at one specific
  `apps/rapid` boundary before ever reaching the CLI (§1.5 row 17 correction).
- `crates/kernel::turn::guard::TurnSubmissionGuard` — exclusive per-session turn occupancy with
  optimistic `expected_seq` conflict detection already exists (165 kernel tests pass) — this is
  substantially what Phase 2 §2.6 (`SessionLease` + fencing) asks for. Not independently re-verified
  whether it's actually exercised by `apps/rapid`'s `resume`/`fork` paths, or only by kernel's own tests.
- `crates/security::scanners::secrets::FindingFingerprint` — a stable content-hash identity (digest
  over rule/path/range/match) for one `Finding` type already exists — substantially what Phase 2 §2.9
  (persisted, content-hash-keyed findings) asks for, at least for secrets scanning specifically.

**Practical consequence for whoever picks up a Phase 2 item below: spend 15 minutes grepping for the
primitive before writing new code.** The likely real task is "wire crate X into `apps/rapid`," not
"build X" — a smaller, safer, and differently-shaped piece of work than the item's original prose
describes. Phase 2 §2.1–§2.10 below were written before this pattern was discovered mid-pass and were
not individually re-audited against source with the same rigor as the Phase 1 corrections above (§2.6
and §2.9 got a quick spot-check, noted inline; §2.2–§2.5, §2.7, §2.8's `RouterDecisionRecord` half, and
§2.10 did not). Treat every remaining Phase 2/3 row as a hypothesis to verify, not a confirmed gap.

## 0. Where RapidLM actually stands today (read this before the tables below)

`gaps.md` is a living document and parts of it are now stale. Commit `ac66e8a` ("Wire the gaps.md parity
core into the exec loop", 28 Aug) already closed several items that document's own tables still list as
open: per-call tool-result messages, parallel batch dispatch, the six-mode permission lattice, and
`AGENTS.md`/`.claude`/`.cursor` rule discovery. Commit `687c745` (29 Aug, this session's baseline) then
wired context-engine retrieval into the exec loop. **Do not re-derive priorities from `gaps.md`'s
executive-summary table without cross-checking current source** — this document's Phase 1 has already
done that cross-check. Confirmed current state, direct from source:

- 15 model-callable tools (`apps/rapid/src/exec_tools.rs`: `workspace.write`, `workspace.read`,
  `repo.read`, `repo.search`, `workspace.patch`, `repo.glob`, `todo_write`, `plan_enter`, `plan_exit`,
  `job_status`, `job_output`, `task_spawn`, `ask_user`, `web_fetch`, `shell_exec`).
- Batched concurrent dispatch (reads parallel, same-path writes serialize) and per-call tool-result
  messages — closed in `ac66e8a`.
- Six-mode permission lattice (`apps/rapid/src/permissions.rs`: `Default | Plan | AcceptEdits | Auto |
  DontAsk | BypassPermissions`).
- `shell_exec` has opt-in macOS Seatbelt sandboxing (`find_sandbox_exec()`, `sandbox-exec -f <profile>`) —
  **macOS only**, no Linux backend.
- `task_spawn` exists; read-only "specialist" subagents cannot inherit the parent's capability lease
  (`agent_runtime::specialist::PersistentSpecialist::inherits_parent_lease()` hardcoded `false`) — but
  there is **no write-scoped, narrowly-leased child run path at all** (`PersistentSpecialist::new()`
  rejects any non-read-only role outright).
- Context-engine retrieval (FTS + code graph + Context Scout) is now reachable from `rapid exec` for
  trusted workspaces (`apps/rapid/src/context_retrieval.rs`, wired `687c745`) — previously ~92% of that
  crate was dead weight.
- Shadow-verified writes exist for `workspace_write` only (`apps/rapid/src/shadow_diagnostics.rs`,
  `02a583b`) — `workspace_patch` is not covered yet.
- `NodeState::Paused` exists at the graph-scheduler layer (`GraphService::pause`/`resume`,
  `crates/scheduler/src/service.rs`, `02a583b`) — no OS-level process suspension, and `rapid graph` has no
  CLI surface at all despite being listed in `00-README.md`.
- No public distribution: build from source only.

---

## Phase 1 — Close parity against Grok Build and Qwen Code

Items are grouped by dimension. Each row states the gap, which competitor(s) already close it, and where
it lands in RapidLM.

### 1.1 Sandboxing breadth

**Correction (2026-08-29, during implementation):** the original framing of this row was wrong. It is
not "no native Linux sandbox" — `crates/sandbox` already contains a mature, tiered, cross-platform
implementation (`backend.rs`'s `SandboxManager`/`SandboxBackend` trait plus four backends:
`HostRestrictedBackend` — process-group + resource limits, unix/windows via `cfg` —, `ContainerBackend`,
`GvisorBackend` (shells to `runsc`, genuinely stronger isolation than raw Landlock — syscall mediation, not
just filesystem policy), and `RemoteBackend`; ~11,400 lines total). **The real gap is identical in shape
to the context-engine finding from commit `687c745`: the crate is fully built and unwired.**
`apps/rapid` does not depend on the `sandbox` crate at all (absent from `Cargo.toml`, zero `use sandbox::`
call sites). `shell_exec`'s `sandbox: true` path (`apps/rapid/src/exec_tools.rs`, `find_sandbox_exec()`)
is a small, separate, macOS-only inline `sandbox-exec` (Seatbelt) invocation that predates and never
touches `SandboxManager`'s tier system. Neither implementation currently knows the other exists.

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 1 | **Second correction (2026-08-29, later same pass):** wiring `SandboxManager` in turned out to be blocked on a deeper, crate-level gap, not app-level plumbing. `SandboxExecResult`/`ResourceUsage` are deliberately "counters only — never holds output bytes" (doc comment on `ResourceUsage`, `crates/sandbox/src/backend.rs`); traced into `HostRestrictedBackend::run_supervised` and confirmed it already reads the child's stdout/stderr into a buffer (`wait_child`) but only keeps the *length*, discarding the content, before returning `SandboxExecResult`. The other three backends (`container.rs`, `gvisor.rs`, `remote.rs`) share the same result type and almost certainly the same pattern (not independently re-verified for this note). This makes the crate correct for its apparent original use case (pass/fail + resource accounting, e.g. a verification/CI gate) but **unusable for `shell_exec` as-is** — the model needs to see command output, not just an exit code. Fixing this is a real, multi-file crate change (extend `SandboxExecResult` with a bounded output field, thread actual bytes instead of counts through each backend's own read loop, update each backend's existing tests), not a wiring task — scoped down and handed to a dedicated follow-up (see spawned task, this session) rather than rushed. | Landlock + Seatbelt + child seccomp, named profiles | Docker/Podman only (also no native Linux sandbox — this is a real RapidLM opportunity: `GvisorBackend`'s syscall mediation is a stronger property than Qwen's container-only story) | `crates/sandbox/src/backend.rs` (`SandboxExecResult`) + all four `crates/sandbox/src/backends/*.rs`; then add `sandbox` as an `apps/rapid` dependency, register `HostRestrictedBackend` + `ContainerBackend` + `GvisorBackend` behind `SandboxManager`, route `shell_exec`'s non-macOS path through `prepare`/`exec`/`destroy`. Lease minting for `Capability::ProcExec` also needs standing up from scratch in `apps/rapid` — confirmed no production code path anywhere in the repo currently mints a real `CapabilityLease` (only test helpers do, e.g. `crates/computer-use/tests/fixtures.rs`, `crates/capability-broker/src/lease.rs` tests, and `tool-gateway::dispatch` builds the right `ResourceDescriptor`/`ProcessScope` shape but doesn't call `issue()` either) — `LeaseIssuer::ephemeral()` is the right constructor for an in-process, non-persisted key. | P0 | L |
| 2 | No macOS backend registered with `SandboxManager` at all — the existing Seatbelt code and the tiered crate are two disconnected implementations | Landlock + Seatbelt + child seccomp, named profiles | Docker/Podman + macOS Seatbelt (six `.sb` profiles) | Write a `SeatbeltBackend: SandboxBackend` in `crates/sandbox/src/backends/` wrapping the existing `sandbox-exec` invocation (move it from `exec_tools.rs`, don't duplicate it), register it so macOS also goes through `SandboxManager`'s single code path instead of a special case. Note: `SandboxTier`'s four variants (HostRestricted/Container/Gvisor/RemoteWorker) don't have a clean slot for "single-process syscall mediation" — Seatbelt is stronger than bare `HostRestricted` but isn't a container. Land it at `HostRestricted` tier with a doc-comment flagging the taxonomy gap rather than widen `protocol::SandboxTier` speculatively; revisit if a second syscall-mediation-but-not-container backend ever shows up. | P1 | M |
| 3 | Sandbox profile selection isn't policy-coupled to permission mode | Auto-allow keying between sandbox profile and approval mode | Partial (YOLO does **not** imply sandboxing — documented explicitly; don't copy this gap) | `capability-broker` + `sandbox` — gated on #1/#2 landing first | P1 | S |

### 1.2 Multi-agent / subagents

**Correction (2026-08-29, during implementation):** row 4 below was wrong as originally written. It
conflated two unrelated constructs. `agent_runtime::specialist::PersistentSpecialist` (a long-lived,
read-only background *observer* — "Context Curator," §3 of the agent-harness design, never a `task_spawn`
target) is correctly read-only-or-reject; that part of `gaps.md` finding #8 is accurate for *that* type.
But `task_spawn`'s actual runtime (`apps/rapid/src/interactive.rs`'s `LiveSubagentRunner::run`) is a
**separate, write-capable path that already exists**: any `agent_type` other than `"explore"`/`"plan"`
gets `ExecTools::workspace_with_permissions` (full write access), and — this is the real gap — it is
constructed with `self.permissions.clone()`, i.e. **the spawned child gets a byte-for-byte copy of the
parent's entire permission lattice** (mode, rules, persisted grants), not a narrowed one. The problem
isn't "no write-scoped child run exists" — it's "the one that exists doesn't scope down at all."

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 4 | ~~Write-capable `task_spawn` children inherit the parent's full `PermissionLattice` unmodified~~ **Implemented 2026-08-29.** `PermissionLattice::for_subagent()` (`apps/rapid/src/permissions.rs`) caps `BypassPermissions` down to `AcceptEdits` for a spawned child — the one mode with no `Ask` step at all — while carrying rules/grants over unchanged; every other mode already denies non-file-edit calls for a subagent (headless-style execution, no interactive channel, `Ask` renders as denial per `evaluate`'s doc comment), so only that one mode needed a ceiling. Wired into `LiveSubagentRunner::run` (`interactive.rs`). New test `subagent_lattice_caps_bypass_but_leaves_every_other_mode_and_rules_alone`. | Subagents in-loop, `general-purpose`/`explore`/`plan` naming (narrowing depth not fully profiled here) | `general-purpose`/`Explore`/`review-agent`/`fork` subagent types, tool-set hard-restricted per role **at runtime**, not just by prompt | `apps/rapid/src/interactive.rs::LiveSubagentRunner::run` + `crate::permissions::PermissionLattice` | ~~P0~~ done | M |
| 5 | ~~Subagent results return as flat text~~ **Implemented 2026-08-29.** New `SubagentReport` struct (`summary`, `status`, `tool_calls`, `tokens`, `stop_reason`) carried across the `SubagentRunner` trait boundary instead of flattening to a `String` inside `LiveSubagentRunner::run`; `execute_task_spawn` renders the final text from the typed fields. Deliberately only carries what `ExecOutcome` actually produces today — no invented "touched files"/"proposed patches" fields nothing populates (a fuller Modbit-style `AgentResultEnvelope` is still future work, this is the trait-boundary half of it). Test mock and assertions updated to check the structured fields reach the rendered summary, not just the free-text body. | Not established | Structured completion via `functionResponse`/tool-result parts, but no dedicated child-result schema either | `apps/rapid/src/exec_tools.rs` (`SubagentRunner` trait, `SubagentReport`) + `interactive.rs` (`LiveSubagentRunner`) | ~~P1~~ done | M |
| 6 | No competitive/parallel multi-model execution mode | None | **Agent Arena**: 2–5 models race in isolated `git worktree`s, automated `git apply` merge-back of the winner | See Phase 3 §3.1 — do not build this until the Change Engine hardening in Phase 2 §2.1 lands; an automated merge without a real `MergeTransaction` primitive is how you corrupt a tree | P2 | — (gated on §2.1) |

### 1.3 Extensibility

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 7 | ~~Hooks cover only `pre_tool_use`/`post_tool_use`~~ **Implemented 2026-08-29.** Added `session_start` (fires once per `rapid exec` run, right after settings load), `session_end` (fires on *every* exit path of `exec_turn` — early `?`-propagated error, explicit early return, or falling off the end — via a `SessionEndHookGuard` whose `Drop` impl fires exactly once regardless of which path was taken; this was initially skipped as "too risky to intercept every return point" and then actually implemented once the `Drop`-guard approach was worked out, rather than left half-done), `subagent_start`/`subagent_stop` (fire around `task_spawn`) — all four via a new notification-style `run_notify_hooks` (fire-and-collect, never gates, unlike `pre_tool_use`). 6 events total now, up from 2. Still far short of Grok Build/Qwen's ~15 events, no `http`/`function`/`prompt` executor types (command-only, matching the existing `pre_tool_use`/`post_tool_use` shape). | 15 events, blocking semantics | ~15 events across 4 executor types (`command`/`http`/`function`/LLM-judged `prompt`), parallel by default | `apps/rapid/src/hooks.rs`, `interactive.rs` (`SessionEndHookGuard`) | ~~P1~~ done (partial breadth) | M |
| 8 | ~~No cross-CLI config/plugin import~~ **Partially implemented 2026-08-29.** Found and fixed a real inconsistency while scoping this: `exec_permission_lattice` already merged permission rules across *both* `.rapidlm/settings.json` and `.claude/settings.json` (`PROJECT_SETTINGS_FILES`), but the separate block loading hooks/MCP-servers/shadow-diagnostics/fetch-allowlist only ever read `.rapidlm/settings.json` — a `.claude/settings.json`-only project silently lost all four. Extracted into `load_project_integrations()`, which merges list-shaped config (allowlist, each hook stage, MCP servers) across every settings file the same way permission rules already do, and uses first-file-wins for the one single-value config (shadow-diagnostics), matching `exec_permission_mode`'s own precedence. **Still not done:** full plugin-manifest/marketplace import (Claude Code Marketplace plugins, Gemini CLI extensions) — this only closes the settings-file compat gap, not a plugin-package importer; that remains real, separate, larger work requiring an authoritative (not guessed) plugin manifest schema. | Reads `.claude/settings.json` for permission rules only | Converts and installs **Claude Code Marketplace plugins**, Gemini CLI extensions, Qoder plugins; `/import-config claude-code` | `apps/rapid/src/interactive.rs` (`load_project_integrations`, `ProjectIntegrations`) | P1 (partial) | M |
| 9 | MCP tool schemas are not lazily hydrated | Meta-tools (`search_tool`/`use_tool`) | Same idea, plus 20 KB cap on eager injection | `mcp` crate | P2 | S |
| 10 | ~~No SDK-style typed tool-schema export~~ **Implemented 2026-08-29.** New `rapid tools` subcommand (`apps/rapid/src/p9_commands.rs::run_tools_schema`) dumps the model-facing tool surface — every tool's `name`/`description`/`parameters` (already-existing JSON Schema per tool, confirmed via the pre-existing `tool_surface_advertises_all_sixteen_tools_with_json_schemas` test) — as a single versioned JSON document (`rapidlm.tool_surface` schema), analogous to Claude Code's `sdk-tools.d.ts` or Grok Build's protobuf tool API. `--read-only` dumps the narrower surface a subagent's `explore`/`plan` scope gets instead of the full one; `--root <path>` points it at a real project (introspective only, never writes). Note this reuses `ExecTools::tool_surface()`, which already existed — the actual gap was purely "no CLI command exposes it," not missing schema data. | Protobuf tool API (`xai-grok-tools-api`) | `sdk-tools.d.ts`-equivalent not present either | `apps/rapid/src/p9_commands.rs` (`run_tools_schema`) | ~~P2~~ done | S |

### 1.4 Memory & context

**Correction (2026-08-29):** rows 13 and 14 below are narrower than first written — spot-checked
against source during this pass (not as thoroughly as the sandboxing/cost-accounting corrections
above, flagging as lower-confidence rather than re-scoping in full):
- `crates/context-engine/src/compact.rs::compact_packet` already has a `CompactMethod::Deterministic`
  path used as the built-in fallback whenever no model summarizer is supplied or one fails — i.e. a
  mechanical, no-model-call compaction mode already exists at the context-engine layer. What's
  unconfirmed is whether it's reachable as an explicit, user-requested "always mechanical, skip the
  summarizer" mode (row 13's actual ask) rather than only as an automatic fallback.
- A form of session export already exists: `rapid inspect export <session> <file>`
  (`apps/rapid/src/p9_commands.rs::run_inspect_export`) writes raw ledger events as JSONL. Row 14's
  real gap is narrower than "no export exists" — it's the richer `html`/`md` rendered-transcript
  formats, not the JSONL case.
Re-verify both before implementing rather than trusting the original row text.

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 11 | No background memory-consolidation pass | Not established | **"Dream"**: LLM-planned dedup/cleanup over saved memories, daily or on-demand | `context-engine::memory` (crate exists per `gaps.md` remediation item 15 — extend, don't replace) | P1 | M |
| 12 | No git-committed team-shared memory tier | Not established | `.qwen/team-memory/` with a mandatory secret scanner before commit | `context-engine::memory` + `security::scanners::secrets` (a `Finding`-lifecycle type already exists for secrets — reuse it, per `gaps.md` finding #4) | P2 | M |
| 13 | No *user-requested* mechanical (non-model-call) compaction fast-path — the deterministic path already exists as an automatic fallback (see correction above), just maybe not as an explicit mode | Not established | `/compress-fast`: strips old tool output/thinking with no model call | `context-engine::compact` (already has the primitive) + wherever compaction is user-triggered | P2 | S |
| 14 | ~~No rendered (`html`/`md`) export format~~ **Partially implemented 2026-08-29.** `rapid inspect-export <session> <file> --format md` renders a chronological Markdown list (`- **kind** (seq N, timestamp) — \`payload\``) alongside the existing (now-default, unchanged-behavior) `--format jsonl`. Deliberately generic, not per-event-kind prose: `EventKind` has dozens of variants across session/turn/model/tool/... families, and rendering each one's payload into readable sentences is real, separate work this doesn't attempt — the value here is a readable, chronological skim of a transcript without guessing at semantics this function doesn't actually know. **`html` format not done.** | Not established | `/export {html,md,json,jsonl}` | `apps/rapid/src/p9_commands.rs::run_inspect_export` | P2 (partial) | S |

### 1.5 Headless / scripting contract

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 15 | Exit codes are undifferentiated (fail-closed but opaque: `agent turn failed: failed`) | Not profiled in depth | Structured taxonomy: 41 auth · 42 input · 44 sandbox · 52 config · 53 turn-limit · 54 tool-exec · 55 budget · 130 SIGINT | `apps/rapid/src/headless/` | P0 | S |
| 16 | No constrained structured-output mode | Not established | `--json-schema` registers a synthetic tool, Ajv-validated against a caller-supplied schema | `headless` + `tool-gateway` | P1 | M |
| 17 | **Correction (2026-08-29):** "no cost accounting anywhere" was wrong — `llm-router` already computes real per-request cost. `provider.rs`'s `UsageCost::Reported { usd_micros }` is constructed from real responses in both `providers/openai_compatible.rs` and `providers/anthropic.rs` (not just a test fixture), and `ModelDescriptor` (`llm-router/src/provider.rs`) already carries `prices: ModelPrices` inside a full `ModelCatalog` (`llm-router/src/catalog.rs`) with latency class, context limits, regions, data-policy tags — i.e. most of what Phase 2 §2.8 below asks for already exists. **The actual gap is narrower and precisely located:** `apps/rapid/src/model.rs::fold_stream` reads `NormalizedUsage` (which carries `.cost()`) but only extracts `usage_total_tokens(usage)` before constructing `ModelStepOutput` — and `ModelStepOutput` (defined in `agent-runtime`, 10 construction/match sites across `harness` + 4 `agent-runtime` files + 4 `apps/rapid` files) only carries `tokens: u64`, no cost field. Cost is computed, then silently dropped at exactly that boundary, and never reaches `ExecOutcome`/the headless JSON contract. **Not fixed this pass** — widening `ModelStepOutput`'s shape touches a shared crate across ~10 *files*
but, checked more precisely in a follow-up pass, **40+ individual construction/match expressions**
(most of them struct literals in `apps/rapid/src/host.rs`'s own test suite, which would all need
updating since Rust requires every field at a non-`#[non_exhaustive]` struct literal site) — a real,
larger-than-first-estimated, moderate-risk change deserving its own careful pass; see spawned follow-up
task, and note its description undersold the blast radius slightly (said "10 call sites," actual count
is far higher once every construction and match arm is counted, not just the files containing them).
**The side-channel idea below does not actually work as a full fix**, also discovered on closer
inspection: `apps/rapid`'s real model-selection path wraps `ConfiguredModel` in `SelectedModel` (three
variants) and, for a configured fallback chain, in `FallbackChainModel` (wrapping *multiple*
`ConfiguredModel`s) — both dispatch `LiveModelCall::step` polymorphically, and only `ModelStepOutput`
itself (not a side-channel on one concrete type) flows uniformly through every layer regardless of
which concrete backend answered, exactly the same way `tokens` already does. A side-channel on
`ConfiguredModel` alone would miss the fallback-chain case entirely. Widening `ModelStepOutput` is very
likely the *only* correct fix, not just the safer one — noted for whoever picks up the follow-up task. | Usage/cost fields present (`xai-grok-pager/src/headless/cli.rs`) | Token usage only, no pricing table, no `cost_usd` field | `apps/rapid/src/model.rs` (`ConfiguredModel::step`/`fold_stream`) → `host.rs` (`ExecOutcome`) → `headless/jsonl.rs` | P1 | M |

### 1.6 Distribution (product/packaging, not architecture — tracked here for completeness, not gated on it)

| # | Gap | Grok Build | Qwen Code | Sev | Effort |
|---|---|---|---|---|---|
| 18 | No public install path | curl installer + self-updater + DotSlash | npm, Homebrew, Docker, VS Code/Zed extensions, Tauri desktop | P1 | L (packaging/release-eng work, separate track from this roadmap) |

---

## Phase 2 — Leapfrog: Modbit-informed hardening (clean-room reimplementation only)

Every item below cites its Modbit feature ID for traceability. **None of this is Modbit's code or a
Modbit dependency** — RapidLM already has the adjacent primitive in most cases; the task is to harden it
to the rigor Modbit's own architecture decided on, using RapidLM's own crates and data models.

### 2.1 Change Engine hardening + `MergeTransaction`

Modbit: `CHG-001` (Change Engine + Write Barrier, tagged MOAT — single typed, base-revision-bound, atomic,
provenance-recorded change path; no alternate raw filesystem mutation for agents), `CHG-003` (deterministic
edit-match ladder: exact → whitespace-insensitive with offset remap → context suggestion → ambiguity
error — never guess), `CHG-012` (`MergeTransaction`: source/target/base revisions, pre-merge state,
conflicts, resolutions, validation evidence, committed/rolled-back state — the merge itself is reversible
and auditable), `CHG-008`–`CHG-010` (typed `UndoAction`/`UndoPlan`, optimistic-concurrency revert checking
expected hash before reversal, `ReversibilityClass` for irreversible/compensatable actions).

- **Where it lands:** `workspace` crate. `workspace.patch`'s match ladder should be audited against
  `CHG-003`'s ordering (this is also what `gaps.md` remediation item 4 already asked for, citing Grok
  Build's `search_replace`). `CheckpointManager` (already exists, `workspace::checkpoint`) is the natural
  home for typed undo actions.
- **Why this comes first:** every subsequent Phase 2/3 item that touches concurrent or competitive
  execution (`MergeTransaction` above, Arena-style execution in Phase 3) is unsafe without this. Build the
  primitive before anything that calls it.
- **Sev/Effort:** P0 / L.

### 2.2 `AgentExecutionCapsule` + `AgentResultEnvelope` + write-scoped narrow leases

Modbit: `AGT-005` (`AgentExecutionCapsule` — per-agent tools/model policy/permissions/token budget/private
context; share only authorized workspace/evidence/artifacts, never a hidden shared transcript), `AGT-011`
(`AgentResultEnvelope` — typed child result: summary, artifacts, proposed patches, tests, evidence IDs,
assumptions, conflicts, decisions, touched files/symbols, next action — "do not merge children through
prose alone"), `CAP-008` (detached-agent permission ceiling: background agents may consume existing grants
but cannot interrupt for new ones — escalate via the foreground parent).

- **Where it lands:** directly extends the gap Phase 1 §1.2 row 4 named. `capability_broker::lease`
  already has `LeaseIssuer`/`CapabilityLease` with single-use, validated consumption
  (`validator::{LeaseUseGuard, ConsumedLeaseUse}`) — extend it with a **narrow, write-scoped** lease
  variant instead of `PersistentSpecialist`'s current binary read-only/reject-everything-else gate.
  `task_spawn`'s return path should carry a typed envelope, not a text blob — this closes Phase 1 §1.2
  row 5 at the same time.
- **Guardrail (Modbit `AGT-010`, bounded recursive delegation):** nested delegation off by default; an
  explicit max-depth profile only, never unbounded. Also see `REJ-007` below.
- **Sev/Effort:** P0 / M.

### 2.3 `CapabilitySnapshot` / `AuthorizationEpoch` + Policy Compiler

**Verified genuinely absent (2026-08-29):** a targeted grep for `CapabilitySnapshot`/`AuthorizationEpoch`/
a policy-compiler type across `capability-broker` found nothing — unlike most of this section, this one
really is missing, not just unwired. **Narrower nuance found while scoping this:** `exec_permission_lattice()`
(`apps/rapid/src/interactive.rs`) has exactly one call site, inside headless `exec_turn` — the lattice
loads once per process invocation and is fixed for the whole turn, which is already the core property
`CAP-005` asks for (an immutable per-round snapshot, no mid-turn retroactive policy change) for
`rapid exec`'s single-turn case specifically. Whether the interactive TUI's longer-lived, potentially
multi-turn session reloads settings between turns (and so needs an explicit snapshot/epoch type to get
the same guarantee) was not confirmed — its lattice-construction call site wasn't traced in this pass.
Worth checking before building a full `CapabilitySnapshot` type: the exec case may already need only a
name for a property it already has, while the TUI case is the part that might still be genuinely open.

Modbit: `CAP-005` (immutable per-model-round capability snapshot carried through the model event, tool
call, and run step — mid-round policy changes apply next round, never retroactively), `CAP-001` (Policy
Compiler: hard invariants → enterprise/admin → execution profile → user → project → agent, merged into one
monotonic authority where lower-trust layers may only restrict, never widen), `CAP-009` (an optional fast
classifier for ambiguous approvals sits **beneath** deterministic allow/ask/deny rules and can never
override them — contrast with Qwen Code's `auto` mode, which makes its LLM classifier the factory
default; adopt the classifier-as-safety-net idea, not the classifier-as-default posture).

- **Where it lands:** `capability-broker`. RapidLM's 6-mode lattice already has the right shape; this adds
  the missing piece — a frozen, auditable snapshot per turn, and an explicit merge order so a project's
  `.rapidlm/settings.json` can never widen what a user or admin policy already restricted.
- **Sev/Effort:** P1 / M.

### 2.4 `CompletionContract` (tri-state) + `VerificationPlane`

Modbit: `AGT-025`/`VER-004` (model proposes completion; deterministic/evidence/semantic verifiers return
`VERIFIED | REJECTED | INDETERMINATE`; `INDETERMINATE` never equals success), `VER-002` (`VerificationPlane`
— `DeterministicVerifier`, `EvidenceVerifier`, `ChangeVerifier`, `EnvironmentVerifier`, `SemanticVerifier`,
`GoalVerifier` under one orchestration contract; the verifier model itself is never the authority),
`REJ-006` (explicit anti-pattern: fail-open completion verification is rejected outright — a verifier
error, timeout, or invalid output must yield `INDETERMINATE`, never silent success).

- **Where it lands:** this is the single closest fit in the whole document — `gaps.md`'s own finding #7
  already noted RapidLM's goal-completion gate (V3 invariant #8, `Verification`/`Evidence` as first-class
  `NodeKind`s, `rapid evidence show|verify|export`) is structurally the deepest of any of the three
  products reviewed, but flagged that it was unconfirmed whether the verdict model is genuinely tri-state
  today. **First task here is not new code — it's reading `event-ledger`/`harness`'s actual verdict type**
  to confirm or fix the tri-state shape, then auditing every verifier call site for fail-closed behavior
  per `REJ-006`.
- **Sev/Effort:** P0 / S (audit) → M (if the fix is real, not confirmatory).

### 2.5 Structured `PlanGraph`/`TodoState` outside the transcript + stall detection

**Stall detection implemented 2026-08-29** (the `PlanGraph`/`TodoState` half below is not). New
`detect_stall()` (`apps/rapid/src/host.rs`) scans the last `STALL_WINDOW` (6) tool exchanges for an
identical `(tool, arguments)` call repeated at least `STALL_REPEAT_THRESHOLD` (3) times — a model stuck
re-reading the same file with no distinct progress — and, when found, logs a `--verbose` diagnostic
line via the existing `StepDiag` mechanism on `SupervisedModel::step`. Deliberately diagnostic-only:
never fails or alters the turn, and does **not** inject the warning into the model's own context so it
could see and react to it — that's real follow-up work (would need to thread a warning string into the
prompt/context-packet pipeline), not attempted here. Tests confirm exact-repeat detection, that varied
arguments (real progress) don't false-positive even with the same tool name repeating, that two repeats
stays under threshold, and that an old repetition outside the window doesn't count against a turn that
moved on.

Modbit: `AGT-016` (plan nodes carry status, dependencies, owner, evidence requirements, attempts, and
blockers as durable state outside the transcript — compaction cannot silently change task truth),
`AGT-017` (detect repeated read/edit cycles with no progress and surface the known state plus the
blocker, rather than looping autonomously).

- **Where it lands:** `scheduler` already models this at the graph-node level (`NodeState` now includes
  `Paused` per `02a583b`) — the gap is a **model-visible projection** of plan/todo state that survives
  compaction, not a new state machine. `todo_write` tool exists; wire its state to the graph rather than
  keeping it prompt-only.
- **Sev/Effort:** P1 / M.

### 2.6 `SessionLease` + fencing generation

Modbit: `AGT-018` — a single active mutation owner for a session, with a lease generation counter that
rejects a stale writer. Named explicitly to "prevent desktop/CLI/cloud dual-resume corruption."

- **Where it lands:** directly relevant to RapidLM's daemon/handoff ambitions (`handoff` crate,
  `crates/kernel`'s daemon/IPC). Build this before `rapid resume`/`rapid fork` are exposed across more
  than one concurrent surface — otherwise two clients resuming the same session is a real corruption path,
  not a hypothetical one.
- **Sev/Effort:** P1 / M.

### 2.7 Context Pack Compiler / Workspace Capsule + Next-Edit-Ripple + retrieval-before-edit guardrail

**Correction to the correction (2026-08-29):** the "verified genuinely absent" note directly below was
itself wrong — a methodology bug, not a re-check of source: the grep only covered
`crates/context-engine/src/*.rs` (one directory level), missing `crates/context-engine/src/index/
graph.rs` entirely. **`CodeGraph::impact()` already exists there** — an incoming-only BFS over
callers/importers of a given symbol, hop- and result-bounded, fully implemented — which is exactly
Next-Edit-Ripple. Like nearly everything else in this document, it's unused outside its own file (grep
for `.impact(` across `apps/`/`crates/*/src` confirms zero external call sites). The real remaining work
is: resolve a file-level edit to the `SymbolLocator`(s) `impact()` needs (it takes a symbol, not a file
path), and expose the result as a new model-facing tool or an automatic post-edit annotation — genuine,
moderate-sized wiring work, not a "build a traversal algorithm" task as the paragraph below still assumes.
Lesson for future passes: a "not found" grep result is only as good as its glob — check subdirectories
before writing "genuinely absent" anywhere in this document.

Also newly confirmed: `context-engine::compact`'s whole compaction system (`compact.rs` + `compact_policy.rs`) is
itself unwired from `apps/rapid` — `compact_packet` is only ever called from within
`compact_policy.rs`, in the same crate, never from the exec loop. So Phase 1 §1.4 row 13's "explicit
fast-path" framing undersells it: compaction isn't reachable *at all* today, deterministic or model-based.

Modbit: `CTX-013`/`CTX-014` (a bounded, provenance-carrying, task-specific context package for execution
and handoff — "a context package grants no permissions"), `CTX-017` (tagged MOAT — graph-driven affected
files/symbols/tests/config as change-impact follow-up after an edit, revision/evidence bound), `CTX-003`
(tagged MOAT — retrieval-before-edit: a material edit requires adequate relevant context to already be
retrieved; surface *inadequate context* as a distinct condition rather than editing blind).

- **Where it lands:** RapidLM's V3 invariant #12 ("subagents receive minimal typed task/context envelopes,
  not the full parent transcript") already states this goal — `context_retrieval.rs`'s new
  `PreservedLiveContext`/`CompileInput` types are the concrete substrate to formalize into a named,
  reusable capsule. Next-Edit-Ripple builds directly on `context_engine::index::graph::CodeGraph`, which
  `gaps.md` finding #5 already confirmed exists and is scout-reachable but unused for call-site validation.
- **Sev/Effort:** P1 / M.

### 2.8 Model router upgrade: capability catalog + `RouterDecisionRecord` + real cost accounting

**Correction (2026-08-29):** the capability-catalog half of this is already substantially built —
see the correction on Phase 1 §1.5 row 17 above. What's genuinely missing is `RouterDecisionRecord`
(no auditable routing-decision record exists) and getting the already-computed cost value from
`llm-router` out to a caller (see that same correction for the exact boundary where it's dropped).

Modbit: `MOD-003` (a model capability catalog — context window, tool/parallel/vision/reasoning/
structured-output support, latency, cost, health — "do not route by model name alone"), `MOD-004`
(`ModelCapabilityVector` + `TaskFingerprint`: hard constraints first, then soft optimization for predicted
quality/latency/cost/reliability), `MOD-005` (`RouterDecisionRecord` — requested model, resolved/fallback
model, routing reason, policy version, estimated vs. actual cost — "routing must be auditable"),
`AGT-028` (exact per-run/per-step cost and token accounting).

- **Why this is worth prioritizing over other Phase 2 items:** it's a clean, uncontested lead. Neither
  Grok Build (single provider, no router) nor Qwen Code (5 providers, real adapters, but no dollar-cost
  field anywhere in its own headless contract — confirmed in this session's Qwen Code audit) has this.
  RapidLM's `llm-router` crate already has the filter/score/fallback shape neither competitor matches;
  this closes the one piece it's missing.
- **Where it lands:** `llm-router` (catalog file per `gaps.md` remediation item 20, citing Grok Build's
  `xai-grok-models/default_models.json` as the closest open reference for the catalog *shape*, not its
  content) + `headless` (surface `cost_usd` in the JSON contract, closing Phase 1 §1.5 row 17).
- **Sev/Effort:** P1 / M.

### 2.9 Persisted, content-hash-keyed review findings + `PatchPolicyGate`

Modbit: `VER-007`/`VER-008` (a structured `Finding` model — category, severity, confidence, rationale,
rule, evidence provenance — where dismissed/resolved findings survive reruns keyed by content hash, so a
rerun doesn't resurface something already triaged, but changed/new findings are never hidden), `VER-009`
(`PatchPolicyGate` — security/license/attribution/secret/static/test gates run before commit/merge, and
their results become evidence, not just a console warning).

- **Where it lands:** `security` crate already has a working `Finding`-lifecycle type for secrets scanning
  (`security::scanners::secrets::Finding`, cited in `gaps.md` finding #4) — extend its shape to cover
  general review findings and key persistence by content hash rather than by line number (line numbers
  shift; content hashes don't).
- **Sev/Effort:** P2 / M.

### 2.10 Scoped Credential Broker + Resource Governor

Modbit: `WRK-016` (mint short-TTL, audience/run/workspace-scoped credentials for authorized tools/workers
— hosted provider API keys never leave the gateway), `WRK-017` (CPU/RAM/disk/network/token/cost/
concurrency ceilings — and explicitly: "budgets cannot convert failed verification into success").

**Correction (2026-08-29): the Credential Broker half is already built.** `crates/auth/src/broker.rs`'s
`SecretBroker` already does exactly this — opaque `ScopedSecret` handles (never plaintext), resolution
only at the executor/provider boundary via `SecretBroker::open`, one-use tokens that can't be replayed,
never written to the event ledger/traces/telemetry/logs. Not used anywhere in `apps/rapid` today (which
talks to `auth::InMemoryCredentialStore` directly for the single-provider-credential case it currently
has) — another wiring gap, not a missing feature, and lower priority than the others in this document
since `apps/rapid` doesn't yet have a scenario (MCP server secrets, multi-tenant credential sharing)
that actually needs the scoping `SecretBroker` provides. **The Resource Governor half was genuinely
absent, and still mostly is** — no CPU/RAM/disk/network/concurrency ceiling type exists anywhere in the
workspace (only `GoalBudget`'s narrower turn/token/time budget for one goal, `agent-runtime`, unrelated).
Building a full one is real, standalone systems work needing actual OS-level resource monitoring, not
attempted here. **Implemented 2026-08-29, the one ceiling that doesn't need OS-level monitoring:**
`rapid exec --max-wall-time <seconds>` (`apps/rapid/src/interactive.rs`, `spawn_wall_time_watchdog`) —
a background thread that cancels the turn's existing `CancellationToken` (the same cooperative signal
Ctrl-C already sends, checked by every model step and tool call) once the deadline passes. CPU/RAM/disk/
network/concurrency ceilings remain real, separate future work.

- **Where it lands:** Credential Broker: wire `SecretBroker` into `apps/rapid`'s credential path once a
  real multi-secret scenario exists — premature before that. Resource Governor: new work, `sandbox` or a
  new small crate; encode its "budgets can't buy a fake pass" anti-pattern into whatever implements
  `CompletionContract` in §2.4.
- **Sev/Effort:** P2 / M (broker wiring) + L (governor, new feature).

---

## Phase 3 — Bets that put RapidLM ahead of both, not just even

These are gated on Phase 2 landing first — each one is unsafe or hollow without the primitive it depends
on, noted inline.

### 3.1 Governed competitive multi-model execution (RapidLM's answer to Agent Arena)

Qwen Code's Agent Arena (2–5 models racing in isolated `git worktree`s, automated merge-back of the
winner) has no equivalent in either Rust tool and is a genuinely useful idea — but Qwen's own
implementation applies the winning diff via `git apply` with no typed transaction, conflict record, or
rollback evidence. Modbit's `CHG-012` `MergeTransaction` (§2.1) is the harder version of the same
primitive. **Once §2.1 lands**, RapidLM can offer the same competitive-execution UX with an actually
auditable, reversible merge — something neither existing implementation has. Bound it per Modbit
`REJ-007` (no unbounded parallel agents — competitive execution is earned by explicit task separability
and a fixed, small agent count, not a knob users crank up).

- **Sev/Effort:** P2 / L. **Do not start before §2.1 and §2.2 both land.**

### 3.2 Governed two-tier background automation

`gaps.md`'s own translated-findings section (§20, finding #4) already flagged this as a real gap: RapidLM's
`rapid cron` runs durable background prompts, but nothing distinguishes a "propose, never auto-apply"
tier from full execution. Modbit's `AGT-008` (agent parking — a parent interruption parks child work
rather than cancelling it) and `CAP-008` (§2.2 above) are the supporting primitives. Combine with the
existing `security::scanners::secrets::Finding` pattern (§2.9) to give background automation a native
"reviewable suggestion" output shape instead of inventing a new one per feature.

- **Sev/Effort:** P2 / M. Depends on §2.2.

### 3.3 Verified-success-per-token as a tracked, reported metric

RapidLM's own product thesis (`00-README.md`: "improve verified task success per token") is already,
independently, the same idea as Modbit's `CTX-002` Context Economy Engine ("optimize task-relevant
information per model token and verified outcome... do not sacrifice correctness for compression"). Right
now this is a stated goal with no instrumentation. Once §2.4 (tri-state completion) and §2.8 (real cost/
token accounting) both land, this becomes a computable number RapidLM can actually report per run — turning
a slogan already in the README into a real, differentiating metric neither Grok Build nor Qwen Code
publishes.

- **Sev/Effort:** P2 / S once §2.4 and §2.8 land.

---

## Explicit guardrails — do not adopt these, from either competitor or from Modbit's own rejected list

- **No fail-open completion verification** (Modbit `REJ-006`). A verifier error, timeout, or invalid
  output is `INDETERMINATE`, never a silent pass. This directly shapes §2.4.
- **No unbounded parallel/recursive agents** (Modbit `REJ-007`, `AGT-010`). Depth and fan-out are explicit,
  bounded, off-by-default settings — not a dial users are encouraged to max out. This shapes §2.2 and §3.1.
- **Don't make an LLM-judged auto-approval classifier the factory default.** Qwen Code ships `auto` as its
  out-of-box posture; Modbit's own design (`CAP-009`) explicitly keeps a similar classifier subordinate to
  deterministic policy as a fallback, never the default gate. RapidLM's default should stay `Default`
  (ask), with the classifier idea (if built at all) arriving strictly under §2.3 as an optional layer
  beneath the existing deterministic rules — never replacing them.
- **No scope creep into a general IDE/desktop product.** Most of Modbit's `IDE-*`, `WEB-*` (beyond
  existing computer-use ambitions already in `00-README.md`), and backend-control-plane items (`DAT-*`,
  `IDN-*`, `CTL-*`) describe a different product shape (an Electron/Code-OSS fork with a hosted control
  plane) and are out of scope for a CLI/TUI/daemon tool. They're excluded from this document on purpose,
  not by oversight.
- **No local SLM dependency of any kind** (Modbit `SLM-001`–`SLM-003`, cancelled in Modbit's own roadmap
  for good reason — it doesn't survive contact with real provider-neutral routing). RapidLM's
  provider-neutral `llm-router` is already the right shape; don't regress it by baking in a local-model
  special case.

---

## Follow-up-task diagnostic notes

**`computer-use` fixtures hang (spawned task `task_81037fda`):** ran all 8 tests individually with
`--test-threads=1` (all pass, ~0.00–0.01s each), then all 8 together with default parallelism via
`cargo test -p computer-use --test fixtures --quiet` (also passes, 0.01s total) — reproduced cleanly
twice. This rules out a deterministic concurrency bug in the fixtures file itself with reasonable
confidence: every individual test is fast and correct, and the whole file passes together in isolation.
The original hang only manifested inside a full `cargo test --workspace` run, which spawns many test
*binaries* concurrently across every crate on a machine already under heavy, sustained background CPU
load (several long-running, unrelated processes pinning multiple cores for days — see the note in this
document's own commit history about compile times). The likely cause is resource contention/exhaustion
under that combined load, not a bug in this specific test file. Whoever picks up that task should
prioritize reproducing it via a full `cargo test --workspace` run (accepting the 10-20+ minute cost) over
further scrutiny of `fixtures.rs` in isolation, and consider whether the fix belongs in test
infrastructure (bounded parallelism, e.g. `cargo test --workspace -- --test-threads=N`) rather than in
`computer-use`'s own code.

## Source ledger

- This session's three-way parity research (RapidLM vs. Grok Build vs. Qwen Code), published as an
  artifact this conversation, and the fresh Qwen Code source audit behind it (clone at `265e7f1`).
- `gaps.md` (this repo) — source-grounded Grok Build audit at `9684fa3`, cross-checked against current
  RapidLM source rather than trusted at face value (see §0).
- `docs/benchmarks/2026-08-28-qwen-code-vs-rapid.md`, `docs/research/feature-inspiration-matrix.md`,
  `docs/v2-archive/research/competitive-analysis.md` — prior internal research, lower resolution than the
  above two but consistent with them.
- `/Users/mohsin/useful /Modbit_Feature_Inspiration_Provenance_2026-08-18_v2.md` — Modbit's own
  feature-by-feature provenance matrix (~280 IDs, each tagged LOCKED / PROVISIONAL / EXPERIMENT /
  REJECTED / DEFERRED against a named inspiration source). This document mines the CLI/kernel-relevant
  subset only; the wider Modbit dossier (`Modbit_Goal_Runtime_V2_COMPLETE.md`,
  `Modbit-Consolidated-Architecture-Decision-Record-2026-08-08(2).md`, `Modbit_Lite_TASKS_v5.md`, and the
  `/Users/mohsin/modbit` source tree itself) was not read for this pass and may contain further
  implementation-level detail worth a follow-up mining pass if any Phase 2 item above needs more precision
  than its Modbit ID alone provides.
