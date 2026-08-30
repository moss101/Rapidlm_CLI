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

**A second, opposite pattern also showed up, and matters just as much for anything touching the
interactive TUI specifically:** `run_interactive`'s `SessionLoop` (`apps/rapid/src/interactive.rs`) only
ever drives `InProcessKernelClient`'s session/turn *ledger bookkeeping* — `submit_turn_sync`
(`crates/kernel/src/client.rs:420`) validates a lease and appends a `TurnStarted` event, nothing more.
There is no live agent-turn/tool-dispatch loop in that path at all: `crates/kernel` has zero references
to `ToolDriver`/`PermissionLattice`/`ExecTools`/`AgentExecutor` and doesn't even depend on `agent-runtime`.
Confirmed while scoping §2.3's CAP-005 half, and it explains the `/compact` no-op §1.4/#13 already found
independently (`KernelApi::Dispatch => {}`) — both are the same root cause, not two unrelated bugs. Any
future TUI-touching item in this document should assume "the live chat session doesn't actually run
turns yet" as a starting fact, not verify it fresh each time.

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
| 1 | ~~Second correction: wiring `SandboxManager` in turned out to be blocked on a deeper, crate-level gap~~ **Output-capture half implemented 2026-08-29.** `SandboxExecResult` gained an `output: Vec<u8>` field (sibling to `exit`, not folded into `ResourceUsage` — that struct's "counters only" doc comment is specifically about itself, not its parent). `HostRestrictedBackend`, `ContainerBackend`, and `GvisorBackend` (all three actually reachable — `RemoteBackend::exec` is an intentional stub that always returns `Err(HealthFailed)`, "No Firecracker transport in this protocol task," so it had nothing to fix) each had an internal `join_output`/`wait_child` pair that already read the child's stdout+stderr into a real `Vec<u8>` buffer via `read_capped`, then threw the buffer away and returned only its byte *count* — all three shared byte-for-byte identical logic (a copy-paste common ancestor), so the same fix applied cleanly to each: `join_output` now returns the combined, already-capped bytes instead of a length; `WaitOutcome`'s variants carry `output: Vec<u8>` instead of `output_bytes: u64`; the existing `ResourceUsage` byte counter is now derived from `output.len()` at the one point it's still needed, keeping that struct's own "counters only" property intact. `cargo build -p sandbox --tests` and the crate's full test suite both pass with the real byte-for-byte identical fix applied three times. ~~Still not done, and this is genuinely the app-level wiring task now, not a hidden crate gap:~~ **Wiring implemented 2026-08-30.** New `apps/rapid/src/sandbox_exec.rs` module: `sandbox` added as an `apps/rapid` dependency; `SandboxManager` registers `HostRestrictedBackend` only (deliberately — `ContainerBackend`/`GvisorBackend` need a real tier-selection/degrade policy, tracked as its own follow-up, not guessed here); `run_sandboxed(root, argv, timeout, output_limit)` builds a `SandboxSpec` (mounting `root` read-write at a fixed `workspace` `RepoPath` target and setting `cwd` to that same target — this is the `cwd`/`mount` relationship flagged as unresolved in the reconnaissance note below, now resolved), mints a real single-use `Capability::ProcExec` lease via the full capability-broker ceremony (`PolicyDocument::parse_toml` → `PolicyStack` → `ActionRequest` → `evaluate` → `request_approval` → `resolve(Approve(Once))` → `issue`, using `LeaseIssuer::ephemeral()` — the first production code path in the repo to mint a real `CapabilityLease`; previously only test fixtures did), then runs `prepare`/`exec`/`destroy`. `exec_tools.rs`'s `execute_shell` routes `sandbox: true` through it on the non-macOS-or-no-`sandbox-exec` fallback path, synchronously, returning exit code / timeout / captured combined output. Hit and fixed one real bug along the way, not a crate gap: `HostRestrictedBackend` deliberately rejects a relative or bare `argv[0]` (`relative_executable_is_rejected` is an existing, intentional test) — unlike `std::process::Command`, it does no implicit `$PATH` search — but real `shell_exec` calls pass bare/relative names (`"ls"`, `"./script.sh"`, matching `exec_tools.rs`'s own existing tests), so `sandbox_exec.rs` now resolves `argv[0]` itself (absolute passthrough, root-relative for a path containing `/`, `$PATH` search otherwise) before handing the request to the sandbox crate. All 5 of `sandbox_exec`'s own tests, the full `sandbox` crate suite (83 tests), and the full `rapid` lib suite (254 tests) pass. **Deliberately not done:** `ContainerBackend`/`GvisorBackend` registration (tier-selection policy, item #2 below is the closest open slot for that decision), unifying this synchronous path with the macOS Seatbelt path's async job-based one (see the module's own doc comment).
**Reconnaissance done this pass** (dependency added, backends registered, then reverted rather than leave
an unused half-wired dependency — see below): `HostRestrictedBackend::new()` / `ContainerBackend::new()` /
`GvisorBackend::new()` all take no arguments and can be registered unconditionally — `SandboxManager::select`
already health-checks each and only picks an available one, so no platform-conditional registration logic
is needed. The real remaining unknown is `SandboxSpec`'s `cwd`/`mount` model: `cwd` takes a `RepoPath`
(*repo-relative*, not a host path) and `SandboxSpecBuilder::build()` requires it (`SandboxError::Empty`
otherwise); the actual host directory is supplied separately via `.mount(SandboxMount { source:
CanonicalHostPath, target: RepoPath, mode })` — getting the `cwd`/mount relationship right for a real
project root (as opposed to the existing tests' temp-workspace fixtures) needs to be worked out carefully,
not guessed, before wiring `shell_exec` through it. | Landlock + Seatbelt + child seccomp, named profiles | Docker/Podman only (also no native Linux sandbox — this is a real RapidLM opportunity: `GvisorBackend`'s syscall mediation is a stronger property than Qwen's container-only story) | `crates/sandbox/src/backend.rs` (`SandboxExecResult`, done) + `apps/rapid` (wiring, done) | P0 (done) | L |
| 2 | ~~No macOS backend registered with `SandboxManager` at all~~ **Backend implemented and tested 2026-08-30; `apps/rapid` wiring deliberately not attempted yet — see below.** New `crates/sandbox/src/backends/seatbelt.rs::SeatbeltBackend: SandboxBackend`, registered at `SandboxTier::HostRestricted` exactly as this row anticipated (`IsolationStrength` auto-derives from tier, so no `protocol::SandboxTier` widening was needed at all — mechanical, not a judgment call). Reuses `host_restricted.rs`'s own already-tested mount/cwd-resolution and forbidden-host-source logic (`resolve_cwd`, `resolve_existing_dir`, `is_forbidden_host_source`, all made `pub(crate)` for this) rather than risk a second, subtly different copy of security-relevant path validation — this is genuinely different from this session's usual "duplicate a trivial 3-line helper" pattern precisely because this logic isn't trivial (docker-socket/home-dir/`.ssh`/`.aws`/keychain prefix checks, symlink-safe canonicalization). Builds its own `.sb` profile per prepare (temp file, cleaned up in `destroy`): deny-all-writes then allow only the resolved read-write mount roots plus `/dev/`/`/private/tmp/`, matching `exec_tools.rs::seatbelt_profile`'s existing shape — but only `SandboxNetwork::None` is accepted, and for that case the profile also adds `(deny network*)`, a real capability neither the existing job-based Seatbelt path nor `HostRestrictedBackend`'s process-policy-only isolation has today. This rule ordering (`deny` before a trailing `(allow default)`) was verified empirically against the real `sandbox-exec` binary on this dev machine before writing any Rust — a `(deny network*)`/narrow `(allow file-write* (subpath ...))` earlier in the profile is NOT undone by `(allow default)` later, confirmed via direct `sandbox-exec` invocations (curl DNS failure with the deny present vs. HTTP 200 without it; a real write outside the allowed subpath returns `Operation not permitted`). Tested with 11 new tests including two real, non-mocked security-property tests only possible because this dev environment is macOS: `prepare_exec_destroy_confines_writes_to_the_mounted_root` (a write inside the mount succeeds, a real write to a path outside it is denied by the OS, not by any of this backend's own bookkeeping) and `network_is_genuinely_denied_not_just_unrequested` (a real `curl` to a real host fails under the profile). Full `sandbox` crate suite (94 tests, up from 83) and `cargo build --workspace --tests` both pass. **Deliberately not attempted:** wiring `apps/rapid` to actually use this backend instead of `exec_tools.rs`'s existing async job-based Seatbelt path — that would mean giving up the background-job/poll-via-`job_status` model for sandboxed `shell_exec` on macOS in favor of this backend's synchronous contract, a real user-visible behavior change (not just an internal refactor) that deserves its own scoping and test-migration pass, not a rider on this one; `apps/rapid/src/sandbox_exec.rs`'s own doc comment already flagged this exact split as deliberately deferred. `ContainerBackend`/`GvisorBackend` registration (tier-selection policy) also remains untouched, per item #1's own note. | Landlock + Seatbelt + child seccomp, named profiles | Docker/Podman + macOS Seatbelt (six `.sb` profiles) | `crates/sandbox/src/backends/seatbelt.rs` (backend, done) + `apps/rapid` (wiring into `execute_shell`, not done) | ~~P1~~ P1 (partial) | M |
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
| 9 | ~~MCP tool schemas are not lazily hydrated~~ **Partially implemented 2026-08-30: the 20 KB cap half.** `apps/rapid/src/exec_tools.rs::full_surface_impl` unconditionally appended *every* MCP-registered tool's full schema to the model-facing surface (confirmed: no size check anywhere in the loop) — a misconfigured or adversarial MCP server could advertise arbitrarily many tools with arbitrarily large schemas, re-sent on every model request for the rest of the turn. Added `MAX_MCP_TOOL_SURFACE_BYTES` (20 KB, matching Qwen's own cited number), enforced first-registered-wins: once the cumulative name+description+schema size crosses the cap, later registrations are skipped (a `--verbose`-gated stderr note reports how many). **Not attempted: the actual lazy-hydration redesign** (`search_tool`/`use_tool` meta-tools replacing eager injection with on-demand discovery) — that's a real, separate protocol change to how the model discovers and invokes MCP tools at all, not a bounding fix; scoping it well needs a real decision about the discovery-tool contract and whether directly-advertised tools coexist with indirected ones, not attempted here. | Meta-tools (`search_tool`/`use_tool`) | Same idea, plus 20 KB cap on eager injection | `apps/rapid/src/exec_tools.rs` (cap, done) + `mcp` crate (lazy hydration, not done) | P2 (partial) | S |
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
| 11 | No background memory-consolidation pass | Not established | **"Dream"**: LLM-planned dedup/cleanup over saved memories, daily or on-demand | `context-engine::memory` (crate exists per `gaps.md` remediation item 15 — extend, don't replace) | P1 | M |. **Correction (2026-08-30): confirmed gated on the same "crate is real, `apps/rapid` never calls it" foundation gap found repeatedly elsewhere in this document, checked directly rather than assumed.** `context_engine::memory::MemoryStore` is a real, complete, tested SQLite-backed store (`MemoryWrite`/`MemoryRecord`/`MemoryQuery`, TTL/confidence/provenance fields) — but `grep -rl "context_engine::memory" apps/rapid/src` returns nothing: zero call sites. `apps/rapid` only ever persists memory as the flat `.rapidlm/MEMORY.md` pointer file (`host.rs::load_memory_index`, §1.4 row 12's own git-committed-team-memory correction). A consolidation *pass* over memories that are never actually written to the structured store this item names has nothing to consolidate — this is the identical shape to §3.3's `GoalUsage`/`GoalDriver` finding (a real mechanism, a real crate, and a turn loop that never calls into it), not a new, independent gap. Wiring `context_engine::memory` into `apps/rapid`'s actual write/read paths is itself the real prerequisite, undocumented as its own item anywhere in this file; re-scoping this row's true blocker accordingly rather than leaving the M-effort estimate implying "just add a consolidation job" is the missing piece.
| 12 | ~~No git-committed team-shared memory tier~~ **Partially implemented 2026-08-30: the "mandatory secret scanner" half only.** `.rapidlm/MEMORY.md` (`apps/rapid/src/host.rs::load_memory_index`) turned out to already BE this codebase's git-committed, team-shared memory tier — confirmed not gitignored (`git check-ignore` on it returns nothing), always loaded into every turn's context. What it lacked was Qwen's "mandatory," i.e. blocking, secret scan: `workspace_write` already ran the secrets scanner over every write, but only ever advisory (append a note, never refuse). Added `exec_tools.rs::is_team_memory_path` + a gate at the top of `execute_write`: when the target is exactly `.rapidlm/MEMORY.md`, a non-dismissed finding now returns `ToolStepResult::Failed` (write never touches disk) instead of an advisory note — verified before writing, same "never touch the real tree on a failed check" discipline shadow diagnostics already uses. A `rapid findings dismiss`-ed fingerprint still unblocks the write, reusing the exact same `FindingsStore` every other scanner in this file shares. Every other file keeps the advisory-only behavior; this is a narrow escalation for one specific path, not a policy change to `workspace_write` generally. **Explicitly not attempted:** the actual `.qwen/team-memory/`-style directory tier (multiple files, not one pointer file), and `context-engine::memory`'s SQLite-backed store remains completely unwired from `apps/rapid` (zero call sites, confirmed by grep) — this fix operates entirely at the `.rapidlm/MEMORY.md` text-file layer, not that structured store. | Not established | `.qwen/team-memory/` with a mandatory secret scanner before commit | `context-engine::memory` + `security::scanners::secrets` (a `Finding`-lifecycle type already exists for secrets — reuse it, per `gaps.md` finding #4) | ~~P2~~ P2 (partial) | M |
| 13 | No *user-requested* mechanical (non-model-call) compaction fast-path — the deterministic path already exists as an automatic fallback (see correction above), just maybe not as an explicit mode | Not established | `/compress-fast`: strips old tool output/thinking with no model call | `context-engine::compact` (already has the primitive) + wherever compaction is user-triggered | P2 | S |. **Correction (2026-08-30): "wherever compaction is user-triggered" turned out to be nowhere real, and the gap is bigger than S.** The TUI already advertises a `/compact` slash command (`crates/tui/src/commands.rs:532`, dispatches `KernelAction::CompactSession`) and there's already a `SessionLifecycleIntent::Compact`/`CompactSessionIntent` type for it (`crates/tui/src/session_actions.rs`) — traced the whole path expecting to find *some* existing hook into `compact_packet` to expose the `Deterministic` method on. There isn't one: `CompactSession` isn't specially handled anywhere; `KernelAction::kernel_api()` (`crates/tui/src/commands.rs:827`) falls through its catch-all to `KernelApi::Dispatch` for it, and `apply_kernel_action`'s match on that (`apps/rapid/src/interactive.rs:2015`) is `KernelApi::Approve | KernelApi::Dispatch => {}` — a silent no-op, not even a stub message. Typing `/compact` today does literally nothing. Deeper still: `crates/context-engine::compact::compact_packet(packet: &ContextPacket, ...)` operates on a `ContextPacket`, and grepping confirms `ContextPacket` has zero references anywhere in `crates/tui` or the interactive TUI session loop in `apps/rapid/src/interactive.rs` — the *only* place in the whole app that builds one is `exec_turn`'s one-shot `build_live_context` (used for `rapid exec`, a single non-interactive turn, not the persistent TUI session). So making `/compact` do anything real isn't "expose an existing primitive as an explicit mode" (the original S-effort framing) — it first needs the TUI's live session loop to track a `ContextPacket`-equivalent at all, which today it structurally does not. That's real, separate architecture work (state the persistent session's context blocks somewhere `apply_kernel_action` can reach), not a small wiring fix. Re-scoping to M/L and flagging the `/compact` no-op as its own concrete, verified bug independent of this item's original ask.
| 14 | ~~No rendered (`html`/`md`) export format~~ **Implemented 2026-08-29, completed 2026-08-30.** `rapid inspect-export <session> <file> --format md` renders a chronological Markdown list (`- **kind** (seq N, timestamp) — \`payload\``) alongside the existing (now-default, unchanged-behavior) `--format jsonl`. Deliberately generic, not per-event-kind prose: `EventKind` has dozens of variants across session/turn/model/tool/... families, and rendering each one's payload into readable sentences is real, separate work this doesn't attempt — the value here is a readable, chronological skim of a transcript without guessing at semantics this function doesn't actually know. **`--format html` added 2026-08-30:** same generic chronological-list rendering as `md`, as a minimal static page (`<!doctype html>` + a `<ul>` of `<li>` entries); every field (event kind, timestamp, JSON payload) goes through a new `html_escape()` — ledger payloads are untrusted-origin text (tool output, model text) and this file may be opened in a real browser, so escaping isn't optional even though it's a local file, not a network-facing surface. Caught and fixed one real regression while adding this: an existing test, `inspect_export_rejects_an_unknown_format`, used `"html"` as its example of a format that *should* be rejected — updated to `"xml"` instead, since `html` is now valid. | Not established | `/export {html,md,json,jsonl}` | `apps/rapid/src/p9_commands.rs::run_inspect_export` | ~~P2~~ done | S |

### 1.5 Headless / scripting contract

| # | Gap | Grok Build | Qwen Code | Where it lands | Sev | Effort |
|---|---|---|---|---|---|---|
| 15 | ~~Exit codes are undifferentiated (fail-closed but opaque: `agent turn failed: failed`)~~ **Correction + fix, 2026-08-30.** The premise was half-wrong: `apps/rapid/src/headless/jsonl.rs` already has a real typed taxonomy, `JsonlExitCode` (`Success=0, Usage=2, Policy=3, Provider=4, Runtime=5, GoalIncomplete=6, Sandbox=7, ResourceExhausted=8, Interrupted=130`), derived from `ErrorCode`/`RapidErrorClass`, and — despite its doc comment saying "for `rapid run --jsonl`" — it was already reused by `InteractiveOutcome::exit_code()` for the plain (non-JSONL) `rapid exec`/TUI path too, not JSONL-gated as the doc comment implied. **What was actually still undifferentiated:** 17 separate call sites across `interactive.rs` (`goal` subcommand family, model-configuration errors, a permission-lattice setup failure) and 2 in `p9_commands.rs` (`cron remove`, `agents` inventory) returned a bare, untyped `Ok(1)` instead of routing through `JsonlExitCode` — including the two richest cases: a `Cancelled` (Ctrl-C) turn and a tool-failure stop both exited `1`, identical to every other failure, losing exactly the SIGINT-vs-failure distinction Grok Build's own taxonomy calls out. Fixed by mapping every site to the existing taxonomy (no new variants): "no active goal"/"no evidence recorded"/bad model override → `Usage`; a rejected goal-completion claim or an unmet `complete` precondition → `GoalIncomplete` (a perfect existing fit, not a new bucket); ledger/goal-file IO failures → `Runtime`; a permission-lattice setup failure → `Policy`; a `Cancelled` terminal status → `Interrupted`; a provider-classified `FailureCause` (`Auth`/`Connection`/`Rejected`/`Transient`) → `Provider`; everything else (tool-failure stops, `FailureCause::Unspecified`) → `Runtime`. New `exec_turn_exit_code()` helper in `interactive.rs`; the `Err(AgentExecutionError)` arm now reuses its existing `error_code()` method rather than a second hand-rolled mapping. Three integration tests (`configured_model_integration.rs`, `exec_diagnosability.rs`) were asserting the old flat `1` and were updated to the new, more precise codes — a genuine behavior change (a CI script keyed on exit code 1 for these cases needs updating), not a test-only fix. **Not attempted:** a finer-grained taxonomy matching Grok Build's exact numbering (separate `auth`/`config`/`turn-limit`/`budget` codes) — RapidLM's existing buckets are coarser by design (`Provider` covers auth+connection+rejection+transient; `Runtime` covers tool-exec+unspecified) and widening them is a bigger, separate call, not bundled into this fix. | Not profiled in depth | Structured taxonomy: 41 auth · 42 input · 44 sandbox · 52 config · 53 turn-limit · 54 tool-exec · 55 budget · 130 SIGINT | `apps/rapid/src/headless/jsonl.rs` (taxonomy, pre-existing) + `apps/rapid/src/interactive.rs`, `apps/rapid/src/p9_commands.rs` (wiring, done) | ~~P0~~ done | S |
| 16 | ~~No constrained structured-output mode~~ **Implemented 2026-08-30.** `rapid exec --json-schema <path>` reads and compiles the schema up front (a bad path or malformed schema fails typed as `JsonlExitCode::Usage`, before any model call), then wraps the turn's `ToolDriver` with a new `apps/rapid/src/structured_output.rs::StructuredOutputTools<T>` decorator — adds one synthetic tool (`emit_structured_result`, `parameters` = the caller's schema verbatim) to the advertised surface and delegates every other tool call to the wrapped driver unchanged (including its own `execute_batch` override, so the inner driver's real concurrency for its own calls isn't lost). A call to the synthetic tool is validated against the compiled schema (via the `jsonschema` crate, new dependency, `default-features = false` — the default features pull in `reqwest` for remote `$ref` resolution, which this never needs since the schema is always a fully-inline caller-supplied document, not fetched); a mismatch is a `ToolStepResult::Failed` the model can see and correct, never a silent pass. On a successful call, the validated JSON is captured and printed to stdout in place of the model's own text summary. If the turn finishes without ever producing a valid call, that's a typed `JsonlExitCode::Runtime` failure, not a silent 0 — the constrained-output contract wasn't met. **Not attempted:** the `tool-gateway` crate mentioned in the original "where it lands" — this is `rapid exec`-only (one-shot headless), not wired into the interactive TUI or MCP-server-exposed tool surface. | Not established | `--json-schema` registers a synthetic tool, Ajv-validated against a caller-supplied schema | `apps/rapid/src/structured_output.rs`, `interactive.rs` (`exec_turn`) | ~~P1~~ done | M |
| 17 | ~~No cost accounting anywhere~~ **Implemented 2026-08-29.** Correction chain, then a full fix: "no cost accounting" was wrong from the start — `llm-router` already computed real per-request cost (`UsageCost::Reported`, from real `openai_compatible`/`anthropic` provider responses) and a full `ModelCatalog` with pricing. The value was silently dropped at `apps/rapid/src/model.rs::fold_stream`, which read `NormalizedUsage` but only extracted token count before constructing `ModelStepOutput` (`tokens: u64` only, no cost field). Initially deferred as too large — widening `ModelStepOutput` touches a shared `agent-runtime` type across (checked precisely) 40+ construction/match expressions, and a side-channel workaround doesn't work because `SelectedModel`/`FallbackChainModel` dispatch polymorphically. **Done anyway**, once the risk was reassessed: adding a struct field makes the compiler enumerate every missed site as a compile error, not a silent bug — `cargo build --workspace --tests` was used as the authoritative fix-list rather than tracking sites by hand. `ModelStepOutput::{Terminal,ToolCalls}` now carry `cost_usd_micros: Option<u64>`; `fold_stream` reads the real value via a new `usage_cost_micros()` (no fabricated estimate the way tokens has one — a guessed dollar figure is a lie, not an estimate); a new `CostAccumulator` (`host.rs`, mirrors the existing token counter) sums it across steps, tracking whether *any* step ever reported one so "unknown" never reads back as "confirmed zero"; `ExecOutcome` and `SubagentReport` both carry the total; the `--verbose` diagnostic line and the `tokens used:` stderr line both surface it (`format_usd_micros`, 6 decimals). Still not done: `RouterDecisionRecord` (§2.8 below). **Correction (2026-08-30):** `session_finished` now does have a real call site (`rapid exec --jsonl`, §2.8's own 2026-08-30 note below) — but cost still isn't surfaced there: neither `JsonlRecord::session_finished`'s `data` nor the new `assistant_message` carry `cost_usd_micros`, only `stderr`'s `tokens used:` line does. Widening `session_finished`'s `data` to include it (a golden-JSON-breaking change, deliberately not bundled into the `--jsonl` wiring pass) is real, separate follow-up. **Follow-up implemented 2026-08-30:** `JsonlRecord::session_finished` now takes a `cost_usd_micros: Option<u64>` parameter and always emits the key (`null` when no step ever reported a real cost, never a fabricated `0` — same discipline `CostAccumulator` itself already uses). `interactive.rs`'s one call site widened its `(text, code)` match result to a `(text, code, cost_usd_micros)` triple, reading `outcome.cost_usd_micros` from the same `ExecOutcome` the `tokens used:` stderr line already reads (`Err` arms — no `ExecOutcome` to read from — pass `None`). Confirmed as the anticipated golden-JSON-breaking change: `GOLDEN_SESSION_FINISHED` updated (`cost_usd_micros` sorts before `exit_code` alphabetically, matching the `router_decision` golden test's own already-documented key-order lesson); the two integration-test assertions in `exec_diagnosability.rs` index into `data.exit_code` directly rather than comparing the whole object, so they needed no changes. `assistant_message` still doesn't carry cost (it never claimed to — cost belongs to the turn, not the text) and remains untouched. | Usage/cost fields present (`xai-grok-pager/src/headless/cli.rs`) | Token usage only, no pricing table, no `cost_usd` field | `apps/rapid/src/model.rs`, `host.rs` (`CostAccumulator`, `ExecOutcome`), `exec_tools.rs` (`SubagentReport`), `interactive.rs`; `crates/agent-runtime/src/turn.rs` (`ModelStepOutput`) | ~~P1~~ done | M |

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
- **Correction (2026-08-30): the "audit workspace.patch's match ladder" pointer targeted the wrong
  crate, and the match-ladder half is now fixed independently of the L-effort transaction migration.**
  `crates/workspace/src/transaction.rs` already implements almost exactly `MergeTransaction`
  (`WorkspaceTransaction`/`TransactionManager`, base-revision staleness checks, `rollback`/
  `rollback_on_failure`, pluggable `VerificationHook`s) and is wired into `agent-runtime`'s
  `MergeHandoff`/`ResultStore`, `event-ledger`, `kernel::session::fork`, and `acp::v1` — but **none of it
  reaches `apps/rapid`** (zero `use workspace::` in `exec_tools.rs`). The actual match ladder Grok Build's
  `search_replace` compares against lives entirely in `apps/rapid/src/exec_tools.rs::execute_patch`
  (`crates/workspace/src/patch/model.rs`'s `SemanticPatch`/`PatchOp` is byte-range-based, not text-search,
  so it has no ladder to audit — a different mechanism from what `WORKSPACE_PATCH_TOOL` actually uses).
  **Implemented the two missing tiers in `execute_patch` itself:** tier 2, whitespace-insensitive fallback
  (`find_whitespace_insensitive`/`lines_match_loosely`) — when the exact substring match finds zero
  occurrences, retries line-by-line with each line's whitespace-split tokens compared instead of its raw
  text, tolerant of reindentation/reflowed spacing but never of an actual content difference; applies the
  same unique-vs-ambiguous-without-`replace_all` policy as the exact tier, and splices `new` in verbatim
  at the located byte range (deliberately never reindents the replacement to match the matched region's
  real indentation — that is a second, harder, separately-risky problem this doesn't attempt). Tier 3,
  context-suggestion (`suggest_closest_line`, advisory only) — when even the loose tier finds nothing,
  names the single existing line most similar to `old`'s first line by shared-token overlap, so the model
  has something concrete to correct on retry instead of a bare "not found". **Still not done:** migrating
  `apps/rapid`'s actual writes onto the already-built, already-wired-elsewhere `WorkspaceTransaction` (the
  genuinely L-effort, atomicity/provenance/multi-file half of this item) and the `UndoAction`/`UndoPlan`/
  `ReversibilityClass` types (`CheckpointManager`/`RewindOp`/`RewindPreview` in `workspace::checkpoint`
  already give optimistic-concurrency-checked, previewable, reversible undo, unnamed as such — a
  wiring/naming task, not a build-from-scratch one, but not attempted in this pass).

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
- **`AGT-010` audited and fixed 2026-08-30 — real, exploitable unbounded nesting found, not hypothetical.**
  Checked whether this guardrail already held given how much of §2.2 had already landed. It only half did:
  `WorkspaceTools::open_read_only` (the `explore`/`plan` agent-type path) already excludes `task_spawn`
  from its surface as a side effect of filtering to `ToolKind::Read` tools — confirmed by an existing test,
  `depth 1 enforced`. But `task_spawn`'s *other* branch — any non-`explore`/`plan` agent type, which gets
  a full write-capable `WorkspaceTools` via `open_with_permissions` — kept `task_spawn` in its own surface,
  and `open_with_permissions` always builds a *fresh* `Arc::new(AtomicU64::new(0))` for
  `subagent_spawns` (the `MAX_SUBAGENT_SPAWNS_PER_TURN` counter from §2.10's own correction) rather than
  inheriting the parent's. Combined, this meant: a write-capable subagent could itself call `task_spawn`,
  its child could too, and so on — unbounded in *depth*, with each level getting its own fresh budget of
  32 (`32^depth` total possible spawns, not 32), the exact shape `AGT-010` explicitly names ("never
  unbounded"). **Fixed:** new `WorkspaceTools::disable_nested_spawn()` (mirrors `read_only`'s own
  surface-plus-execution double guard exactly — unadvertised in `tool_surface()`, refused in
  `execute_call_traced()` if called anyway) called on every subagent child's own tools in
  `LiveSubagentRunner::run` (`interactive.rs`), for both the read-only and write-capable branches alike —
  the read-only branch already got this for free from its `ToolKind::Read` filter, but calling it there
  too costs nothing and removes the asymmetry. This closes only the "off by default" half of `AGT-010`;
  the "explicit max-depth profile" half (an opt-in, configurable way to allow depth > 1) does not exist
  and was not attempted — the default is now correctly bounded, but there is no profile mechanism to widen
  it deliberately if a future use case needs to. New test
  `write_capable_subagent_children_cannot_spawn_further_subagents`: confirms `task_spawn` is present
  before `disable_nested_spawn()` (proving the bug was real, not already-impossible), absent after, and
  that a call attempted anyway under `BypassPermissions` (chosen specifically so no *other* gate would
  have denied it first) is refused with an `AGT-010`-labeled reason. Full `-p rapid` suite (304 lib tests)
  and `cargo build --workspace --tests` pass.
- **A third instance of the same "child starts fresh instead of inheriting" shape, found while checking
  what else a subagent's tools default to that the parent's don't: project hooks.** `WorkspaceTools::
  open_with_permissions` defaults `hooks: HooksConfig::default()` (empty), and — same as the disk/network
  budgets — nothing in `LiveSubagentRunner::run` ever called `set_hooks` on the child. A configured
  `pre_tool_use` hook (a real policy-enforcement surface: a security scanner, an approval webhook, a
  linter gate) protects the parent's own tool calls but was silently bypassable by asking a subagent to
  make the same call instead — delegation as a policy-evasion vector, not merely a resource-accounting
  gap like the previous two instances. **Fixed:** new `WorkspaceTools::hooks_config()` (clones the
  configured `HooksConfig`, mirroring `turn_budget_handles()`'s shape) read from the parent right where
  `LiveSubagentRunner` is constructed, stored on it, and applied via the *already-existing* `set_hooks`
  on every child `LiveSubagentRunner::run` builds — no new setter needed, unlike the budget fix. New test
  `subagent_children_inherit_the_parents_policy_hooks`: a `pre_tool_use` hook that denies every call,
  confirmed to *not* gate an unshared child's own tools (the vulnerability was real) and confirmed to gate
  a child with the parent's hooks propagated (fixed). Full `-p rapid` suite (306 lib tests) and
  `cargo build --workspace --tests` pass. `session_start`/`session_end` hooks are deliberately not
  propagated — those fire once per `rapid exec` process, not per tool call, so a subagent (which runs
  inside the same process, not a new one) firing them again would be a duplicate, not a fix.
- **A fourth instance, found by checking the rest of `WorkspaceTools`'s per-instance config for the same
  shape: the shadow-diagnostics quality gate.** Same defect as hooks, one config field over:
  `shadow_diagnostics: None` by default, only ever set on the parent (`tools.set_shadow_diagnostics(shadow)`
  in `interactive.rs`), never propagated by `LiveSubagentRunner::run`. A subagent's `workspace_write` calls
  silently skipped the verify-in-an-isolated-worktree check the parent's own matching writes went through
  — lower severity than the hooks bypass (a quality gate, not a security control, and shadow diagnostics
  already fails open by design on its own misconfiguration), but the same "delegation quietly drops a
  policy the parent had" shape. **Fixed** with the same technique: new `WorkspaceTools::
  shadow_diagnostics_config()` (clone, mirrors `hooks_config()`), read from the parent alongside `hooks`
  right where `LiveSubagentRunner` is constructed, applied via the already-existing `set_shadow_diagnostics`
  in `LiveSubagentRunner::run`. New test `subagent_children_inherit_the_parents_shadow_diagnostics_gate`:
  a `grep -q MARKER {path}` gate that fails a markerless write, confirmed to *not* fire for an unshared
  child (the gap was real) and confirmed to fire once propagated (fixed). Full `-p rapid` suite (307 lib
  tests) and `cargo build --workspace --tests` pass. **Checked and deliberately left alone:**
  `fetch_allowlist` (private-host allowlist for `web_fetch`) has the identical "parent-only" shape, but
  fixing it would *widen* a subagent's capabilities to match the parent's, cutting against this
  codebase's established direction of narrowing subagent scope (write-scope confinement, permission-mode
  capping, nested-spawn denial) — an unshared, empty allowlist makes a child strictly *more* restricted
  than the parent, which is the safe direction to leave a gap in, not one that needs closing.
- **Correction + partial fix (2026-08-30):** `AgentResultEnvelope`'s exact field list already exists —
  `crates/agent-runtime/src/agent/model.rs`'s `AgentResult` carries `summary, evidence, workspace_view,
  patch_summary, artifacts, claims, open_questions, blockers, context_lineage` (all with accessors) —
  but `apps/rapid/src/interactive.rs`'s `LiveSubagentRunner::run` only ever read `.summary()` and
  `.status()` before constructing `SubagentReport`, silently discarding claims/blockers/open_questions/
  patch_summary before they ever reached the parent model's tool-result text. **Fixed the discard, not
  the type shape:** `SubagentReport` (`apps/rapid/src/exec_tools.rs`) gained `claims: Vec<String>`,
  `blockers: Vec<String>`, `open_questions: Vec<String>`, `patch_summary: Option<String>` — pre-rendered
  as text lines (`"{criterion}: {text} ({result})"`, `"[{kind}] {summary}"`, etc.) at construction time,
  matching this codebase's existing convention of flattening typed enums to `String` at the tool-result
  boundary (e.g. `stop_reason`) rather than smuggling a second JSON-typed channel through a text-only
  tool result. `execute_task_spawn` now appends non-empty ones to the returned summary (`\nclaim: ...`,
  `\nblocker: ...`, etc.), so a parent model actually sees what a subagent asserted, was blocked by, or
  left open instead of only its prose summary. **Deliberately not done:** `artifacts()`
  (`Vec<ArtifactRef>`, content-addressed blob refs with a `RedactionClass`) — surfacing these needs a
  real decision about redaction-aware rendering (a `Secret`-class artifact ref probably shouldn't even
  be named in plain text) that a straight `.to_string()` would get wrong by default, so left untouched
  rather than guessed at.
- **`artifacts()` closed 2026-08-30 — checked `protocol::ArtifactRef`'s actual shape and the redaction
  worry doesn't apply.** `ArtifactRef` is `{ id: ArtifactId, media_type: String, bytes: u64, redaction:
  RedactionClass }` — a content-addressed SHA-256 hash, a generic media type, a byte count, and the class
  itself; there is no name/path/locator field at all, so no field carries the artifact's actual content or
  anything content-derived beyond its hash. Naming a `Secret`-class ref this way discloses nothing the
  reference architecture wasn't already designed to disclose (the point of a content-addressed reference
  is that a party can hold and pass it along without ever seeing the payload). `SubagentReport` gained
  `artifacts: Vec<String>`, rendered uniformly across every `RedactionClass` as `"{id} ({media_type},
  {bytes}B, {redaction})"` — the class itself is shown, not hidden, so a parent model reading a subagent's
  result can see *that* a secret artifact exists and its size without ever seeing what it contains.
  `execute_task_spawn` appends `\nartifact: ...` lines the same way claims/blockers do. New assertion in
  `task_spawn_report_surfaces_claims_blockers_questions_and_patch_summary` covers a `secret`-class
  artifact by name, confirming the display choice explicitly rather than leaving it implicit. The **narrow
  write-scoped lease variant** and replacing `PersistentSpecialist`
  are both still entirely open — this pass only closed the "typed data exists but gets thrown away"
  half, not the permission-ceiling half.
- **Narrow write scope implemented 2026-08-30 — at the `PermissionLattice` layer, not as a
  `capability_broker::lease` variant.** Traced the intended lease-level design first: it would mean
  routing the *entire* subagent tool-dispatch path through lease validation, which doesn't happen at all
  today (subagents dispatch via `ExecTools::workspace_with_permissions`/`PermissionLattice`, never
  through a capability-broker lease check) — a much bigger integration than this item's own framing
  suggested, since it's not "add a lease variant" but "make lease validation the actual gate for
  subagent tool calls at all." Built the pragmatic, safe equivalent at the layer that *does* already gate
  every subagent tool call: `PermissionLattice` gained `write_scope: Option<String>` (a workspace-relative
  path prefix) and `with_write_scope()`, checked in `evaluate()` **before** every rule/grant/mode —
  including `bypassPermissions`, which otherwise allows everything unconditionally — so a scope ceiling
  can only be narrowed further, never widened, by anything downstream. Scoped deliberately to
  `ToolClass::FileEdit` only: `shell_exec`'s `subject` is joined argv, not a workspace path, so applying
  a path-prefix check to it would silently misfire. `task_spawn` gained an optional `write_scope`
  argument (validated through the same `checked_relative` fail-closed path check every other tool
  argument uses — an escaping `../` path is a handled, model-visible refusal, not a panic or a silent
  no-op), threaded through the widened `SubagentRunner::run` trait to `LiveSubagentRunner::run`, which
  applies it via `.with_write_scope()` on the child's lattice. **Not attempted:** the actual
  capability-broker lease integration this item originally specified, and replacing `PersistentSpecialist`
  — both remain real, separate, larger work; what's implemented here is a genuine safety improvement
  (a subagent confined this way structurally cannot write outside its scope) using the mechanism this
  codebase already has, not a renamed placeholder.

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
- **CAP-005 half resolved by verification, 2026-08-30 — and it surfaced a much bigger finding.** Traced
  the TUI's lattice-construction call site this pass names as unconfirmed: `run_interactive` →
  `run_started_session` builds one `ServiceGraph` service, `KernelRuntime`, and its `SessionLoop` only
  ever calls `CreateSession`/`SubmitTurn`/`Interrupt`/`ForkSession`/`RewindSession`/`SubscribeEvents`
  against `InProcessKernelClient`. `submit_turn_sync` (`crates/kernel/src/client.rs:420`) does nothing
  but validate a turn-lease, append a `TurnStarted` ledger event, and store the lease — no LLM call, no
  tool call, no `ExecTools`/`AgentExecutor`/`PermissionLattice` anywhere in it, and `crates/kernel` has
  zero references to any of those types at all (confirmed by grep), doesn't even depend on `agent-runtime`
  in its `Cargo.toml`. **The interactive TUI has no live agent-turn/tool-dispatch loop yet at all** — typing
  a prompt into the live chat records `TurnStarted` and returns; nothing in-process invokes a model or
  runs a tool for that turn. This is the same shape as the `/compact` no-op found in §1.4/#13's correction
  (dispatched by the TUI, landing on a silent no-op because the underlying mechanism doesn't exist yet) —
  a real, load-bearing architecture gap, bigger than and separate from CAP-005 itself. Given this, CAP-005
  genuinely doesn't apply to the TUI yet: there's no turn execution there for a snapshot to protect.
  **For the one path that does exist (`rapid exec`), checked whether a `CapabilitySnapshot` wrapper would
  add anything real:** `PermissionLattice` (`apps/rapid/src/permissions.rs`) has zero `&mut self` methods
  — no mutation API exists to guard against in the first place, so the "immutable per-round snapshot"
  property already holds unconditionally, not just by convention. A wrapper type here would rename an
  already-total invariant, not enforce a new one — declining to build it, per this document's own standing
  rule against ceremony with no load-bearing behavior behind it.
- **Policy Compiler (`CAP-001`) half: a real instance of the exact pattern already existed one file
  over, extended to the permission-mode domain, 2026-08-30.** `apps/rapid/src/managed_config.rs`
  (`RAPIDLM_MANAGED_CONFIG`) already implements CAP-001's "hard invariants merge, lower-trust layers may
  only restrict, never widen" shape — just scoped to model configuration (`locked_default`,
  `allowed_providers`, `min_reasoning_effort`, each enforced managed > env > user > default). But
  `exec_permission_lattice()` (the actual tool-approval mode/rule resolution) had zero connection to it:
  a project's `.rapidlm/settings.json` or `RAPIDLM_PERMISSION_MODE` could set `bypassPermissions` freely,
  with no admin ceiling at all. **Implemented:** a new `max_permission_mode` field on `ManagedPolicy` plus
  `gate_permission_mode()`, wired into `exec_permission_lattice()` right after mode resolution — narrows
  the resolved mode down to the managed ceiling when it's exceeded (reported via a new
  `GateReportEntry`, `eprintln!`'d as a warning), passes through untouched otherwise, mirroring
  `min_reasoning_effort`'s existing silent-enforcement shape rather than hard-refusing the whole run
  (becoming *more* restrictive is always safe; a model misconfiguration hard-refusal is not the same
  risk). Needed a real permissiveness ranking to compare modes at all — added
  `PermissionMode::permissiveness_rank()`; confirmed from `evaluate()`'s own logic (not guessed) that
  `Plan` is the strictest of all six (denies every write-classified call outright, `PlanModeDeny`), not a
  position in the enum's declaration order (which only matches `MODE_NAMES`'s lookup table). A configured-
  but-unreadable `RAPIDLM_MANAGED_CONFIG` now fails `exec_permission_lattice` closed too, matching
  `load_policy`'s own already-documented invariant that it must never silently become "no policy" — this
  path previously didn't call `load_policy` at all, so the invariant had nothing to apply to. **Not
  attempted, and this is the bulk of `CAP-001` and all of `AuthorizationEpoch`:** the full hard-invariants
  → admin → execution-profile → user → project → agent merge *order* across multiple settings sources
  (today's merge is a flat union of `.rapidlm/settings.json` + `.claude/settings.json` rules, with no
  concept of which layer "wins" a conflict beyond the two special-cased fields above) — real, separate
  design work.

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
- **Audited 2026-08-29 (commit `6ded0d6`) — this note was missing from the file even though that commit's
  own message pointed here; corrected 2026-08-30 to actually say what was found and decided.**
  `crates/agent-runtime/src/evidence.rs::CriterionEvaluator` is the real goal-completion gate (`harness::
  assertions::Verdict` is a different, unrelated binary type for test-harness assertions over exported
  ledger records — not this gate). Confirmed: `CriterionVerdict.satisfied` is a plain `bool`, not tri-
  state, and every error/cancellation path in `evaluate_with_backing` already collapses to `satisfied:
  false` (fail-closed, matching `REJ-006`'s safety bar) rather than ever defaulting to success. **Decision:
  a literal `VERIFIED | REJECTED | INDETERMINATE` enum was deliberately NOT built.** Forcing one in would
  have meant either (a) letting `INDETERMINATE` NOT block completion — violating `REJ-006` outright — or
  (b) making it block identically to `REJECTED`, at which point it's a relabeling with no behavioral
  difference, not a real tri-state gate. Instead: `CriterionUnsatisfied` gained three new reasons
  (`LedgerEventNotFound`/`LedgerEventWrongKind`/`LedgerUnavailable`, replacing one collapsed
  `UnbackedEvidence` for agent-cited ledger references) plus a `retryable()` classifier — `true` only for
  `UnavailableStatus`/`ErrorStatus`/`LedgerUnavailable` (the verifier itself couldn't produce a definite
  result and a rerun with zero new evidence might succeed), `false` for every other reason (`FailedStatus`,
  `MissingEvidence`, etc. — a genuinely new observation is required). The gate's own behavior is
  unchanged either way (`satisfied` stays `false` for both); `retryable` is a caller-facing "try again" vs.
  "this is final" signal layered on top, not a second axis the gate itself consults.
  **Wiring gap found and closed 2026-08-30:** `retryable()` had zero call sites outside its own unit tests
  in `evidence.rs` — the exact "built but unwired" pattern from this document's own §0a meta-finding.
  `apps/rapid/src/goal_host.rs::export`'s per-criterion JSON now includes `"retryable"` (`null` only when
  the criterion is satisfied and has no reason at all); `apps/rapid/src/interactive.rs`'s `goal verify`
  text output now appends `, retryable` to an unsatisfied criterion's line when true. New test
  (`export_emits_snapshot_verdicts_and_attestation`, extended) asserts `retryable: false` for a
  `MissingEvidence` criterion — the `true` branch itself is already covered by `evidence.rs`'s own
  `retryable()` unit tests; constructing a genuine `LedgerUnavailable` scenario through `GoalHost`'s public
  API would need real ledger-resolver-failure plumbing this pass didn't build. **Still open:** no CLI
  surface reads `CriterionVerdicts::allowed()`'s all-satisfied invariant any differently when every
  unsatisfied criterion happens to be retryable (e.g. "try again shortly" vs. a hard stop) — `retryable` is
  visible per-criterion now, but nothing yet rolls it up to a turn-level "retry advisable" signal.

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
- **Model-visible projection implemented 2026-08-30 — the exact "survives compaction, not prompt-only"
  half this item's "where it lands" note asked for.** `todo_write` already persisted durably to
  `.rapidlm/todos.json`, but nothing ever read it back into context — the model only ever saw its own
  todos through the transcript, so compaction (or a fresh `rapid exec` invocation in the same project)
  could lose track of them entirely. Found the fix by tracing how `.rapidlm/MEMORY.md` already achieves
  exactly this for the memory index: `PreservedLiveContext::with_memory_index` feeds a plain string into
  `build_packet` via `CompileInput::new("memory/index", text)` — `CompileInput`, not the closed
  `context_engine::compile::ContextBlock` (an internal-only type the compile pipeline constructs from
  `CompileInput`, with no public constructor of its own — this was the exact wall the stall-detection
  injection hit and correctly declined to force through; todos needed no such wall, since this injection
  point was already open and proven). Mirrored the pattern exactly: new `host::load_todos_index(root)`
  reads and renders `.rapidlm/todos.json` (fail-open on missing/corrupt — persisted plan state is
  advisory context, not something a turn should fail to start over; a malformed individual entry is
  skipped, not fatal to the whole projection), `PreservedLiveContext::with_todos_index`/`todos_index()`
  mirror `with_memory_index`/`memory_index()` field-for-field, and `build_packet` compiles it as a
  `"plan/todos"` system block whenever present. **Deliberately not attempted:** the `scheduler::NodeState`
  graph half — this is the durable, *readable* projection into context, not a structured state machine
  with dependencies/owner/evidence-requirements/attempts that `todo_write`'s flat id/content/status shape
  doesn't carry at all; wiring the stall-detection warning into context (this item's other still-open
  half, `AGT-017`) also remains undone — that one genuinely does need a decision about which turn-loop
  layer computes it in time to feed `build_packet`, since `detect_stall` runs inside `SupervisedModel::step`,
  after context for that step was already compiled, not before it like the todos/memory case.

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

**Next-Edit-Ripple implemented 2026-08-30 — the "genuine, moderate-sized wiring work" the note above
anticipated, not the traversal algorithm (already existed).** Two small, additive `context-engine` reads
closed the "resolve a file to symbols, resolve a symbol back to a file" gap `CodeGraph::impact()` itself
never needed for its own BFS: `CodeGraph::symbols_at_path(repo_id, path)` (a direct indexed query against
`context_graph_symbols`'s existing `path` column — no schema change) and `CodeGraph::symbol_label(locator)`
(resolves one `impact()` edge endpoint back to a `(fq_name, RepoPath)` for display, `None` on any lookup
miss — advisory, never a hard error). New `apps/rapid/src/context_retrieval.rs::ripple_advisory(root, path)`
sits alongside the existing `retrieve()` (same module, same persistent `.rapidlm/index/`, same fail-open/
bounded-timeout philosophy): after a successful `workspace_write`/`workspace_patch`, it walks (stat/
eligibility only, no parsing — bounded by `MAX_RIPPLE_WALK_FILES`) looking specifically for the one
just-written path, indexes only that match, then calls `impact()` on that file's symbols and renders the
callers it finds as an advisory note appended to the tool's own summary — the same shape every other
scanner advisory in `exec_tools.rs` already uses. Deliberately does **not** re-walk and re-index the whole
repo per write the way `retrieve()` does once per turn (a multi-edit turn would otherwise pay a full walk
per edit) — indexing is scoped to the one changed file only. **Important, explicitly documented
limitation:** a caller only shows up if it was already indexed by an earlier `retrieve()` call this
session (persisted across turns, but genuinely absent on a fresh project or if retrieval was skipped) —
this is "what the graph already knows," not an on-demand full dependency audit; verified this precisely
with a test that fails without an earlier `retrieve()` call and passes with one. New tests: two in
`context-engine::index::graph` (`symbols_at_path_finds_only_that_files_symbols`,
`symbol_label_resolves_next_edit_ripple_end_to_end`, the latter exercising the whole
resolve→impact→resolve chain against a real two-file call graph) and one in `apps/rapid`
(`ripple_advisory_names_the_file_that_calls_the_edited_function`, covering the positive case, an
uncalled function producing no advisory, and a nonexistent path failing open). Full `context-engine`
suite (307 tests, up from 305) and full `rapid` lib+integration suite (295 lib tests) both pass, plus
`cargo build --workspace --tests`. **Still not attempted:** `CTX-013`/`CTX-014`'s full Context Pack
Compiler/Workspace Capsule and `CTX-003`'s retrieval-before-edit guardrail (surfacing *inadequate*
context as a distinct condition) — this closes only the Next-Edit-Ripple half of this section.

Modbit: `CTX-013`/`CTX-014` (a bounded, provenance-carrying, task-specific context package for execution
and handoff — "a context package grants no permissions"), `CTX-017` (tagged MOAT — graph-driven affected
files/symbols/tests/config as change-impact follow-up after an edit, revision/evidence bound), `CTX-003`
(tagged MOAT — retrieval-before-edit: a material edit requires adequate relevant context to already be
retrieved; surface *inadequate context* as a distinct condition rather than editing blind).

- **Where it lands:** RapidLM's V3 invariant #12 ("subagents receive minimal typed task/context envelopes,
  not the full parent transcript") already states this goal — `apps/rapid/src/host.rs`'s
  `PreservedLiveContext` (not `context_retrieval.rs` — corrected 2026-08-30; it's the live context
  envelope used throughout `host.rs`/`interactive.rs`/`exec_tools.rs`/`model.rs`) is the concrete
  substrate to formalize into a named, reusable capsule. Next-Edit-Ripple builds directly on
  `context_engine::index::graph::CodeGraph`, which `gaps.md` finding #5 already confirmed exists and is
  scout-reachable but unused for call-site validation.
- **Sev/Effort:** P1 / M.

### 2.8 Model router upgrade: capability catalog + `RouterDecisionRecord` + real cost accounting

**Correction (2026-08-29):** the capability-catalog half of this is already substantially built, and the
cost-plumbing half is now **implemented** — see Phase 1 §1.5 row 17 above for both. What's still
genuinely missing: `RouterDecisionRecord` (no auditable routing-decision record exists), and surfacing
cost in the headless JSONL contract specifically — `apps/rapid/src/headless/jsonl.rs::session_finished`
has no call sites at all outside its own tests, so it isn't wired into `rapid run --jsonl` yet regardless
of cost; extending it is real, separate work once it's wired up at all.

**Correction + partial fix (2026-08-30): `session_finished` now has a real call site, closing half of
the "isn't wired up at all" gap above — but not via `rapid run`.** Tracing where `--jsonl` was actually
supposed to land surfaced a bigger, adjacent finding: `CLI_USAGE`'s own "Target V3 command surface" text
lists dozens of subcommands (`run`, `resume`, `fork`, `rewind`, `daemon`, `acp`, `graph`, `context`,
`evidence`, `process`, `computer`, `mcp`, top-level `hooks`/`plugins`/`skills`, `eval`, top-level
`sandbox`) that `run_subcommand`'s actual match has no arm for at all — `rapid run <goal/playbook>`, the
JSONL contract's own documented home ("`rapid run --jsonl` writes only protocol records to stdout"), is
one of these: entirely unbuilt, not merely unwired. Building the real "durable graph run" feature is
large, separate work. What genuinely was tractable: `rapid exec` — a real, working, already-tested
one-shot flow — reusing the *same* JSONL contract (`headless::jsonl`) for the same underlying purpose
(machine-parseable output + a typed exit code), without pretending this is the full durable-graph `run`
command. **Implemented:** `rapid exec --jsonl` writes `rapid.schema`, then (on the turn's outcome) an
`assistant.message` record carrying the same text the plain path would print (the `--json-schema`
captured JSON when present, else the model's own summary) *only when the turn produced one*, then
`session.finished` with the real typed exit code — reusing `exec_turn_exit_code`/`AgentExecutionError`'s
own `error_code()` mapping built for the plain-text path, so the two paths can never silently disagree
on what code a given failure gets. New `JsonlRecord::assistant_message` constructor (mirrors the existing
`EventKind::ModelCompleted → "assistant.message"` mapping `from_event` already used, byte-identical wire
shape, golden-JSON-tested) and `now_rfc3339()` (new `time` crate dependency — already resolved
transitively in `Cargo.lock`, so this promotes an existing dependency to direct rather than adding a new
one to the supply chain; `formatting`-only, no `parsing`, since this only ever stamps "now"). **Narrower
than full correctness, disclosed rather than silently gapped:** `--jsonl` only covers the turn-execution
outcome — a pre-flight setup failure (bad model config, bad `--json-schema` document, a permission-
lattice error) still exits with the correct typed code but without a `session.finished` record, since no
JSONL writer or session id exists yet at that point in `exec_turn`. Extending every one of those ~15
early-return sites to also finish the JSONL sequence is real, separate follow-up work, not attempted here
to keep this change reviewable. Streaming individual `assistant.delta`/`tool.*` events as the turn runs
(rather than one `assistant.message` at the end) is also not attempted — `exec_turn` only collects
`agent_runtime::TurnEvent`s in-memory, not through a real committed `EventLedger` with durable per-event
seq numbers, so `JsonlRecord::from_event`'s intended ledger-backed path doesn't apply to this one-shot,
non-durable flow at all.

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
- **`RouterDecisionRecord` implemented 2026-08-30.** `apps/rapid/src/host.rs::FallbackChainModel::step`
  already computed exactly the shape `MOD-005` asks for at every retry/fallback/stop decision — it just
  threw the information away into an ephemeral `--verbose`-gated diagnostic string (`diag_line`) instead
  of a structured, queryable record. Added `RouterDecisionRecord` (`requested_model`, `resolved_model`,
  `reason: RouterDecisionReason` — `RetrySame`/`FallbackTo`/`Stop(reason)`) and `RouterDecisionLog`, an
  `Arc<Mutex<Vec<_>>>` handle mirroring `CostAccumulator`'s existing shape: cloned out of the
  `FallbackChainModel` before it's moved into `SelectedModel`/`run_live_exec`, read back in
  `interactive.rs::exec_turn` once the turn resolves. The common case — a turn that never needed to retry
  or fall back — produces zero records, not a record saying so; absence *is* the "used the requested
  model, no incident" signal. Surfaced two ways: unconditionally on stderr (a mid-turn model switch is
  operationally significant enough to show without `--verbose`) and, in `--jsonl` mode, one new
  `router.decision` record per entry (`requested_model`/`resolved_model`/`reason`, the last a short
  machine-stable tag: `"retry_same"`/`"fallback_to"`/a `StopReason::as_str()` value) written after
  `rapid.schema` and before the outcome records, with `seq` kept monotonic across all of them. **Not
  attempted:** `policy_version` and `estimated vs. actual cost` per decision (`MOD-005`'s full field list)
  — no policy-versioning concept exists anywhere yet to cite, and per-decision cost attribution would need
  threading `CostAccumulator`'s per-step values back into whichever attempt they belonged to, not just the
  turn-level total this codebase currently tracks; both are real, separate follow-up.

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
- **Correction + partial fix (2026-08-30): the secrets scanner is more built than the doc credits, and
  now has its first production call site — advisory-only, not the `PatchPolicyGate` this item actually
  asks for.** Beyond `security::scanners::secrets::Finding`/`FindingFingerprint` (a real, mature,
  prefix/pattern-based scanner: AWS keys, GitHub/Slack tokens, private keys, connection strings, high-
  entropy assignments), there are three more parallel typed `Finding`+content-hash-fingerprint scanners —
  `PatchFinding`, `CommandFinding`, `ExternalFinding` (`scanners/patch.rs`, `command.rs`, `external.rs`)
  — all already content-hash-keyed (`VER-007`'s actual ask), all with zero call sites anywhere in
  `apps/rapid`, entirely dormant. **Implemented:** `apps/rapid/src/exec_tools.rs::execute_write`'s plain
  (non-shadow-diagnostics) write path now runs the secrets scanner over newly-written content and appends
  an advisory note to the tool's own success summary when it finds something (`"advisory: possible
  secret(s) detected (rule_id, ...) — verify before committing"`) — deliberately never blocking the
  write: a scanner false positive (e.g. a high-entropy test fixture) must never break a legitimate
  workflow, matching this codebase's model-correctable-not-fatal philosophy (the model sees the note in
  its own tool result and can redact/rewrite if the flag is real). **Explicitly not `PatchPolicyGate`:**
  this is a notification, not a gate — nothing here blocks a commit/merge, nothing becomes durable
  evidence, and the other three scanners (patch/command/external) and the dismiss/resolve/triage
  persistence store this item's title actually names (confirmed: no dismiss/resolved/triage persistence
  exists anywhere in the crate) remain entirely unattempted. Also not covered: `execute_write`'s shadow-
  diagnostics branches and `execute_patch`'s written content — scoped to the one plain-write path to keep
  this change reviewable, not a signal that those paths are exempt from the same risk.
- **Persistence half of `VER-007` implemented 2026-08-30 — the "dismissed findings survive reruns keyed
  by content hash" part specifically, not the full `PatchPolicyGate`.** New
  `apps/rapid/src/findings_store.rs::FindingsStore`: a project-local `.rapidlm/findings.json` (alongside
  `.rapidlm/todos.json` — project-local, not per-user, since a team's triage decisions are project facts)
  mapping a `FindingFingerprint`'s hex straight to a dismissal reason. `scan_for_secrets_advisory` now
  loads it and filters out already-dismissed fingerprints before building its advisory note, so a
  dismissed finding never resurfaces on a rerun of the *same* content — but a changed finding at the same
  location gets a different fingerprint (the hash covers rule/path/byte-range/match-content) and is never
  silently hidden by an old dismissal, exactly `VER-007`'s stated invariant. New `rapid findings
  list|dismiss <fingerprint> --reason <text> [--root <path>]` CLI surface (`p9_commands.rs::run_findings`)
  makes it actually usable, not just inert plumbing — every advisory note itself now also prints the
  fingerprint so there's something to pass to `dismiss`. Caught a real bug in this pass, not just in the
  new code: the advisory message's own wording, `"possible secret(s) detected"`, put an unrelated `(` (in
  `"secret(s)"`) before the fingerprint's own parenthesis — a naive fingerprint-extraction test failed
  silently on the wrong substring until traced; fixed by rewording to `"possible secrets detected"`
  rather than papering over it in the test. **Still not attempted, and this is the bulk of `PatchPolicyGate`
  (`VER-009`) itself:** an actual gate (something that can block a commit/merge, not just advise), the
  three other dormant scanners (patch/command/external) wired the same way, and any concept of "results
  become evidence" (a durable, queryable record of what was checked and why it passed/failed) — a
  dismissal file recording what a human decided is not the same as the gate deciding anything itself.
- **Checked whether `CommandFinding` (the `shell_exec` scanner) is as quick a wire-up as `secrets` was,
  2026-08-30 — it is not, and here is exactly why, for whoever picks this up next.**
  `security::scanners::command::CommandRiskScanner::scan` takes a
  `capability_broker::CanonicalCommand`, not a raw argv `Vec<String>` — and `CanonicalCommand` has no
  public constructor; it's only built via `normalize_exec()` against a real `Resolver` impl (executable
  PATH resolution + cwd canonicalization, the exact ceremony `apps/rapid/src/sandbox_exec.rs::
  resolve_program` already implements standalone for a different purpose). `execute_shell`
  (`exec_tools.rs`) has none of this machinery today — no `Resolver`, no `normalize_exec` call anywhere
  in `apps/rapid`. Wiring this scanner in is thus a real, self-contained integration on the same order as
  building `sandbox_exec.rs` was (a `Resolver` impl, `normalize_exec` call, `ShellMode::Argv` vs.
  `ShellString` handling), not a copy of `scan_for_secrets_advisory`'s shape — noted precisely rather than
  forced through under time pressure. `PatchFinding`/`ExternalFinding` weren't checked with the same
  rigor and may or may not have the same shape; verify each independently before assuming either way.
- **Correction — `CommandFinding` WAS wired in after all, 2026-08-30, reversing the assessment above once
  the integration was precisely scoped.** The "real, self-contained integration on the same order as
  building `sandbox_exec.rs`" turned out to be reusable rather than duplicable: `sandbox_exec::
  resolve_program` (executable PATH/root-relative resolution) was made `pub(crate)` and reused as-is, and
  the `Resolver` ceremony needed only a trivial "already resolved, just validate" impl — copied from
  `p9_commands.rs`'s existing `FrozenPathResolver` as `exec_tools.rs::AlreadyResolvedPathResolver`. New
  `exec_tools.rs::scan_command_advisory(root, argv)`: resolves `argv[0]` via `resolve_program`, builds an
  `ExecIntent`/`CanonicalCommand` via `normalize_exec`, runs `security::CommandRiskScanner::scan`, and
  filters findings through the same `FindingsStore` from the secrets correction above (so `rapid findings
  dismiss <fingerprint>` works uniformly across both scanners — `FindingsStore` is keyed by fingerprint hex
  string, not a scanner-specific type, exactly because it was built generic). Wired into `execute_shell`'s
  plain synchronous path only (not the sandboxed or background paths), appending an advisory note to the
  success summary the same way `scan_for_secrets_advisory` does — never blocking, matching the same
  model-correctable-not-fatal philosophy. Verified with a new `#[cfg(unix)]` test
  (`shell_exec_flags_a_dangerous_command_but_never_blocks_it`, using `rm -rf <path>` to trigger
  `command.rm_destructive`) plus the full `-p rapid` lib+integration suite (292 lib tests, all integration
  binaries) and a full `cargo build --workspace --tests`, all green. **Still not covered:** the sandboxed
  and background `shell_exec` paths (same rationale as secrets' shadow-diagnostics gap — scoped narrow to
  keep the change reviewable), and `PatchFinding`/`ExternalFinding` remain entirely unattempted.
- **Correction — `PatchFinding` wired in too, 2026-08-30, and it needed none of `CommandFinding`'s
  ceremony.** Checked independently as flagged above: `security::PatchScanTarget::create` takes a
  `protocol::RepoPath` directly, no `Resolver`/`normalize_exec` involved — the simplest of the three
  dormant scanners to wire. New `exec_tools.rs::scan_patch_advisory(root, path, content)`: parses
  `args.path` as a `RepoPath` (already relative/traversal-free from `checked_relative`), builds a
  `PatchScanTarget::create(.., executable: false)` unconditionally (this write path has no chmod
  capability, so a target it produces is never actually executable, and the content-scanning rules that
  can fire here — credential paths/material, CI-release paths, sudoers, hook paths — don't distinguish
  `Create` from `Replace`; only `Delete`/`Move` do, and this path never produces either), runs
  `security::PatchScanner`, and filters through the same `FindingsStore` the other two scanners use.
  Wired into `execute_write`'s plain path alongside (not instead of) the secrets scan — a single write can
  now carry both advisory notes. Verified with a new test
  (`workspace_write_flags_a_patch_policy_issue_but_never_blocks_the_write`, writing a `write-all` GitHub
  Actions workflow to trigger `patch.ci_permissions_broaden`) plus the full `-p rapid` lib+integration
  suite (293 lib tests) and `cargo build --workspace --tests`, all green. **Still unattempted:**
  `ExternalFinding`, and the same shadow-diagnostics/`execute_patch` paths the secrets scanner also skips.
- **`CommandFinding`'s "not covered" background/sandboxed paths closed 2026-08-30.** `scan_command_advisory`
  had only ever been wired into `execute_shell`'s plain synchronous path; the macOS Seatbelt job path, the
  non-macOS `sandbox_exec::run_sandboxed` path, and the plain background-job path (`self.jobs.start`) all
  ran a command's real argv unscanned. All three now call the same `scan_command_advisory(self.root(),
  &args.argv)` (the real argv, not the Seatbelt-wrapped `sandboxed` vec that prepends `sandbox-exec -f
  <profile>`) and append the note to their own success summary — `"started sandboxed job ..."`, `"sandboxed
  exit ..."`, `"started background job ..."` all now carry the advisory the same way the plain path's
  `"exit 0 ..."` already did. New `#[cfg(unix)]` test
  (`shell_exec_flags_a_dangerous_command_on_the_background_and_sandboxed_paths_too`) exercises both the
  background and sandboxed paths with a real `rm -rf` against a harmless nonexistent target, on this
  actual macOS dev machine (so the Seatbelt branch, not the `run_sandboxed` fallback, is what's really
  covered here — `run_sandboxed`'s own branch gets no direct test since `find_sandbox_exec()` always
  succeeds on macOS). Full `-p rapid` suite (296 lib tests) and `cargo build --workspace --tests` pass.
  `ExternalFinding` and `execute_patch`'s equivalent command-adjacent surfaces remain the only pieces of
  this section still untouched.
- **`execute_patch`'s own "not covered" gap closed 2026-08-30 too — secrets, patch-policy, and ripple all
  now scan the patched content, not just fresh writes.** Both success tiers of `execute_patch` (exact-match
  and whitespace-insensitive) now run `scan_for_secrets_advisory`/`scan_patch_advisory`/`ripple_advisory`
  against `updated` (the post-patch file content) before returning, in the same order `execute_write`
  already uses. This was flagged as a gap independently in both the secrets correction ("not covered:
  ... `execute_patch`'s written content") and the patch-policy correction ("still unattempted: ... the
  same shadow-diagnostics/`execute_patch` paths") — both close with this one change, since both scanners
  take the same `(root, path, content: &[u8])` shape regardless of which tool produced the bytes. New test
  `workspace_patch_scans_the_resulting_content_like_workspace_write_does` exercises both success tiers:
  a secret introduced via an exact-match patch, and a `patch.ci_permissions_broaden` finding reached only
  through the whitespace-insensitive fallback (confirming the scan runs on that tier too, not just the
  exact-match one). Full `-p rapid` suite (297 lib tests) and `cargo build --workspace --tests` pass.
- **`execute_write`'s shadow-diagnostics branches closed too, 2026-08-30 — every write/patch path in
  `exec_tools.rs` now runs the same three scans.** Rather than repeat the three `if let Some(note) = ...`
  blocks a fifth and sixth time, extracted `append_write_advisories(summary, root, path, content)` — the
  one place all three scanners (secrets, patch-policy, ripple) are called from now, used by
  `execute_write`'s plain path, both its shadow-diagnostics outcomes (`Passed`/`Skipped`), and both
  `execute_patch` match tiers alike, instead of five near-identical call sites drifting independently.
  Scans run *after* shadow diagnostics has already decided to apply the write (still advisory-only,
  appended to the same success summary) — not *before* the shadow-diagnostics candidate is accepted, which
  would be a real, different verify-then-apply semantics change and wasn't attempted. Extended
  `shadow_diagnostics_applies_a_passing_write_for_real` to also assert a secret introduced through that
  path is flagged, not just the plain path. Full `-p rapid` suite (297 lib tests, unchanged count since
  this extended an existing test rather than adding a new one) and `cargo build --workspace --tests` pass.
- **The actual `PatchPolicyGate` (`VER-009`) — the one piece of this whole section repeatedly flagged as
  "the bulk of it remains unattempted" across every prior correction — implemented 2026-08-30 for its
  single most meaningful boundary: `git commit`.** Every scanner wired this session (secrets, patch-
  policy) was advisory-only by design — a note the model sees and can act on, never a block. This item's
  actual ask was different: a real gate that blocks the commit boundary itself. New `exec_tools.rs::
  scan_git_commit_gate(root, argv)`: when `shell_exec`'s plain path is about to run a plain `["git",
  "commit", ...]` call, it reads `git diff --cached --name-only` for the staged file list, `git show
  :<path>` for each file's staged content (not working-tree content — the two can differ), and runs
  `scan_for_secrets_advisory`/`scan_patch_advisory` on each — the exact same scans and the exact same
  `FindingsStore` dismiss mechanism every other scanner in this file already uses, reused verbatim rather
  than reimplemented. Any non-dismissed finding refuses the commit outright (`ToolStepResult::Failed`,
  handled, model-visible) *before the `git commit` process is even spawned* — the real, qualitative
  difference from every other scanner call site, which only ever appends a note to an already-succeeded
  result. Fails open on anything that isn't a real, introspectable git repo with staged changes (a repo
  `git diff --cached` can't run against must not be blocked by a check that can't run) — this can only
  ever narrow which commits succeed, never widen what's allowed. Caught a real bug in the message wording
  while writing the test, the same class as the earlier `"secret(s)"` bug this session already fixed once:
  `"unresolved findings (PatchPolicyGate):"` put a confusable `(` before the finding's own `(fingerprint)`
  parenthesis; reworded to `"blocked by the PatchPolicyGate: staged changes have unresolved findings:"`
  rather than patching around it in the test. Two new tests:
  `git_commit_is_blocked_by_an_unresolved_secret_in_staged_content` (a real git repo, a real staged secret,
  a real blocked commit confirmed via `git log`, then a real successful commit after `rapid findings
  dismiss`) and `git_commit_with_no_findings_is_never_gated` (a clean staged file commits normally). Full
  `-p rapid` suite (301 lib tests) and `cargo build --workspace --tests` pass. **Explicitly not covered:**
  `git commit` wrapped in a shell string (`["sh", "-c", "git commit ..."]`, undetectable since `shell_exec`
  never interprets shell strings) is not gated; and "results become evidence" (a durable, queryable record
  of what was checked, distinct from a dismissal file recording a human's decision) remains the one part
  of `VER-009` genuinely unaddressed — the gate now decides something real, but that decision isn't
  recorded anywhere durable beyond the commit either succeeding or being refused.
- **`git merge` gate implemented 2026-08-30, the same day — the "separate, similarly-shaped follow-up"
  flagged above.** New `scan_git_merge_gate(root, argv)`, sharing the actual scanning (`collect_content_
  findings`, extracted from what was `scan_git_commit_gate`'s inline loop) with the commit gate rather
  than duplicating it — the only real difference between the two boundaries is *which* files count as
  "about to become permanent." A merge has no single "staged" set to read the way a commit does, so the
  target ref(s) are read straight from `argv` (every trailing token after `git merge` that doesn't start
  with `-`; a flag-only invocation like `git merge --continue` has no such token and is correctly left
  alone), the changed-file list comes from `git diff --name-only HEAD <ref>`, and each file's *incoming*
  content is read via `git show <ref>:<path>` — deliberately never the working tree, which an unmerged
  branch hasn't touched yet. Both gates are checked at the same `execute_shell` call site
  (`scan_git_commit_gate(...).or_else(|| scan_git_merge_gate(...))`). New test
  `git_merge_is_blocked_by_an_unresolved_secret_in_the_incoming_branch`: a real second branch with a real
  staged-then-committed secret, a real blocked merge (confirmed by the target file never landing on disk),
  then a real successful merge after `rapid findings dismiss`. Full `-p rapid` suite (302 lib tests) and
  `cargo build --workspace --tests` pass. Both named `PatchPolicyGate` boundaries (`VER-009`: "before
  commit/merge") are now real gates, not advisories — only the durable-evidence half of `VER-009` and a
  shell-string-wrapped invocation of either command remain open.
- **Durable-evidence half of `VER-009` implemented 2026-08-30, the same day — "results become evidence,
  not just a console warning."** New `record_gate_decision(root, boundary, blocked, findings)`: appends
  one JSON object per line to `.rapidlm/gate_log.jsonl` (`{schema, time, boundary: "commit"|"merge",
  blocked, findings}`) after *every* gate check that actually ran — a clean pass as much as a block,
  since "what was checked and why it passed" is as much evidence as "what was checked and why it failed."
  Deliberately minimal rather than a full ledger integration: no new `event-ledger` event kind, no
  `EvidenceRecord`/`EvidenceService` involvement (that system is goal-criterion-scoped, and conflating a
  general security-gate audit trail with goal-completion evidence would blur two genuinely different
  concepts) — just a durable, append-only, greppable/`jq`-able file, matching the same "a plain file is
  the honest tool for this" choice `FindingsStore` itself already made for dismissals. A write failure
  here never affects the gate's own decision — recording is advisory to the gate, not a second gate. New
  test `git_commit_gate_decisions_are_recorded_durably_blocked_and_clean_alike`: a real blocked commit, a
  real dismissal, a real clean commit, then both `.rapidlm/gate_log.jsonl` lines parsed and asserted on
  (`blocked: true` with a non-empty findings array for the first, `blocked: false` with an empty one for
  the second — confirming a dismissed finding doesn't silently resurface in its own evidence record
  either). Full `-p rapid` suite (303 lib tests) and `cargo build --workspace --tests` pass. **What
  remains of `VER-009`:** only a shell-string-wrapped `git commit`/`git merge` invocation, which
  `shell_exec`'s own "no shell string is ever interpreted" design makes structurally undetectable at this
  layer without a much bigger change to how commands are parsed.

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
network ceilings remain real, separate future work needing actual OS-level monitoring.

**Correction (2026-08-30): the concurrency axis was already narrower than claimed, and is now fully
covered for this codebase's actual shape.** `apps/rapid/src/exec_tools.rs::MAX_BACKGROUND_JOBS` (16) was
already a real, pre-existing concurrency ceiling on live `shell_exec background: true` jobs — the "no
concurrency ceiling type exists anywhere" claim above was wrong for that one axis. The genuinely open gap
was `task_spawn`: unbounded, no cap on how many subagents one turn could start — a runaway or adversarial
loop could burn real tokens/cost/wall-time with nothing stopping it. **Implemented 2026-08-30:** a new
`MAX_SUBAGENT_SPAWNS_PER_TURN` (32) enforced via a `subagent_spawns: Arc<AtomicU64>` counter on
`WorkspaceTools`, checked before every `task_spawn` call; once exhausted, the call is a typed, handled,
model-visible failure (`"task_spawn budget exhausted"`), never a hard kill — the runner is never even
invoked past the cap. Named a *total-per-turn* cap rather than "concurrency" deliberately:
`execute_task_spawn` already runs synchronously (blocks until the child turn finishes before the tool
call returns), so there is no actual concurrent-subagent risk in this codebase's design to cap — only an
unbounded-sequential-total one, which is what WRK-017's spirit ("budgets can't buy a fake pass" aside)
is really protecting against here. CPU/RAM/disk/network ceilings are still real, separate, OS-level work.

**Correction (2026-08-30): the CPU/RAM claim was wrong too — real OS-level enforcement already existed
for one path, was silently misconfigured, and is now fixed, not just documented.** While auditing this
section, checked `crates/sandbox/src/backends/host_restricted.rs` (already wired into `apps/rapid` since
an earlier session via `sandbox_exec::run_sandboxed`, the non-macOS `shell_exec(sandbox: true)` path) and
found it already does genuine OS-level CPU (`ulimit -t` via `require_cpu_rlimit`) and memory
(`/proc`-based RSS sampling against `plan.memory_mb`, killing the process group on breach) enforcement —
this is real, not a stub. The bug: `apps/rapid/src/sandbox_exec.rs::build_spec` never set `cpu_millis`/
`memory_mb` on the `SandboxSpec` it built, so every sandboxed `shell_exec` call on that path silently ran
under `SandboxSpecBuilder`'s generic crate-wide defaults — `cpu_millis: 1_000` (**one CPU-second**) and
`memory_mb: 256` — values sized for nothing in particular, certainly not a general-purpose shell command.
**Verified empirically, not assumed:** a real `sh` loop doing ~200,000,000 iterations of trivial
arithmetic was killed by `SIGXCPU` after ~2.4 real seconds with **zero output** and no informative error
— `execute_shell` only ever reported "sandboxed no exit code (signalled)", identical to any other signal
death, giving no hint that a resource ceiling (not a crash, not a kill request) was the cause. This is a
real correctness bug for anyone actually relying on `shell_exec(sandbox: true)` on Linux/Windows: a
legitimate CPU-bound script (a build, a data-processing loop, anything nontrivial) would silently die with
no explanation, not just a "runaway process" edge case. **Fixed 2026-08-30:** `sandbox_exec.rs` now sets
explicit, intentional `SANDBOX_CPU_MILLIS = 30_000` (30 CPU-seconds) and `SANDBOX_MEMORY_MB = 1024`
(1 GiB) on every spec it builds, replacing the accidental generic defaults. Also surfaced the previously-
discarded `SandboxExit::signal()` through `SandboxRunOutcome` (new `signal: Option<i32>` field) and added
`exec_tools.rs::sandboxed_status_line()` (extracted, directly unit-tested) so a future CPU-limit kill
reports `"killed: sandbox CPU-time limit exceeded (SIGXCPU)"` by name instead of the same opaque
"no exit code (signalled)" for every signal death. New tests: `run_sandboxed_survives_a_moderately_cpu_
heavy_command` (a real, wall-clock-bounded ~3-second busy loop that would have died under the old 1-
second default and now completes) and `sandboxed_status_line_names_the_cpu_limit_specifically` (pure
unit test over the four status-line cases). Full `-p rapid` suite (299 lib tests) and
`cargo build --workspace --tests` pass. **What this does not close:** the macOS Seatbelt path (this
session's own new `SeatbeltBackend`, `crates/sandbox/src/backends/seatbelt.rs`) has no CPU/memory
enforcement at all yet — only timeout/cancel — a real, separate gap in that backend specifically, not
attempted here; and disk/network ceilings for the sandboxed exec path itself (distinct from the per-turn
disk/network budgets in §2.10's earlier paragraph, which cover `workspace_write`/`web_fetch`, not
sandboxed shell commands) remain unaddressed.
- **`SeatbeltBackend`'s own CPU gap closed too, 2026-08-30, same session as the backend itself.** Reused
  `host_restricted.rs`'s exact `sh -c 'ulimit -t "$1" || exit 125; shift; exec "$@"'` wrapper technique
  (duplicated rather than shared — a fixed three-line script, unlike the mount/path-validation logic this
  module already reuses from `host_restricted.rs`), applied to the whole `sandbox-exec` invocation:
  `exec` replaces the process image and the rlimit survives it, while `current_dir` (set on the same
  `Command`) is unaffected since `exec` never changes cwd. `SeatbeltPlan` gained a `cpu_millis` field
  captured from `spec.cpu_millis()` in `prepare`. New `cpu_ceiling_kills_a_command_that_exceeds_it_
  before_the_wall_clock_timeout` test, calibrated the same careful way as the `apps/rapid` fix's own test
  (see next paragraph) — a real, measured shell loop that reliably exceeds a 1-second CPU ceiling.
  Memory (RSS) monitoring remains unenforced for this backend — `host_restricted.rs`'s own memory
  enforcement needs a background sampling thread this backend's simpler `wait_child` loop doesn't have;
  not attempted here, a real, separate gap.
- **Correction, same day: the first version of both new CPU tests was itself broken, and silently proved
  nothing — worth recording exactly why, since it's a real testing-methodology trap.** Both this fix's
  first regression test and the `SeatbeltBackend` CPU test originally used a *wall-clock-bounded* busy
  loop (`end=$(($(date +%s)+3)); while [ $(date +%s) -lt $end ]; do :; done`) chosen for "predictable
  duration regardless of shell-arithmetic throughput." That reasoning was wrong: `date +%s` forks a new
  subprocess every iteration, and `RLIMIT_CPU` only counts the CPU time of the *one process it's set on*
  — a parent shell that spends nearly all its wall-clock time blocked in `fork`/`wait` on child processes
  accumulates almost no CPU time of its own, so this loop could run for any number of real seconds
  without ever approaching even a 1-second CPU ceiling. Both tests "passed" under the buggy 1-second
  default too, meaning they verified nothing about the actual fix — caught only by deliberately re-running
  each test against the reverted (buggy) default and noticing it *still* passed, which should never happen
  for a real regression test. Also discovered mid-investigation: a naive large iteration count for the
  *positive* case (200,000,000, guessed from how long it took a `ulimit -t 1`-killed run to receive
  `SIGXCPU`, which is not the same as how long the loop takes to actually finish) turned out to need
  **over 120 real seconds** to complete — confirmed by directly timing `/bin/sh -c '...'` outside any
  sandbox at all. Fixed by measuring real throughput directly (`time /bin/sh -c 'i=0; while [ $i -lt N ];
  do i=$((i+1)); done'` at a few values of `N`) and picking `N = 1,500,000` (~4 real/CPU seconds,
  confirmed by direct timing), a pure shell-builtin loop with no subprocess forking. Re-verified both
  fixed tests fail under the reverted 1-second default and pass under the real one before trusting them.
  Lesson for future sandbox/rlimit tests in this codebase: never trust a resource-ceiling test that
  hasn't been run against a deliberately-broken version of the fix it claims to verify.

**Disk axis, 2026-08-30: a real per-turn ceiling landed without needing any OS-level monitoring —
the same "count what's already flowing through a chokepoint" trick as the concurrency fix above,
not the OS-level work the "still real, separate" note above assumed disk required.** Every
`workspace_write`/`workspace_patch` call already bounds its own content size
(`MAX_WRITE_BYTES` = 64 KB), but nothing bounded the *count* of calls — a runaway loop writing
max-size files repeatedly could consume unbounded disk with no single call ever exceeding its own cap,
exactly the same shape as the `task_spawn` gap already fixed. **Implemented:** `MAX_TOTAL_WRITE_BYTES_PER_TURN`
(64 MB, 1024x the per-call cap — generous enough for any real coding task, tight enough to stop a
genuinely pathological loop) enforced via a `bytes_written: Arc<AtomicU64>` counter on `WorkspaceTools`
and a `reserve_write_budget()` helper (atomic reserve-then-rollback-if-over, safe against two concurrent
near-the-limit writes on different paths racing each other — same-path writes already serialize via
`write_group_key`), checked in both `execute_write` (all three of its write points: the two shadow-
diagnostics branches and the plain path) and both of `execute_patch`'s write points before the bytes
ever reach `fs::write`, so a refused write never touches disk. CPU/RAM/network ceilings still remain
genuine OS-level work — this is disk specifically, and specifically the "unbounded call count" shape,
not a byte-accurate disk-usage monitor (a single `fs::write` beyond the file's own existing size isn't
separately accounted for, e.g. overwriting a large file with a similarly large one; the ceiling is on
cumulative *written* bytes this turn, not net disk delta).

**Network axis, 2026-08-30: same shape, applied to `web_fetch`.** `MAX_TOTAL_FETCH_BYTES_PER_TURN`
(16 MB) enforced via a `fetch_bytes: Arc<AtomicU64>` counter and `reserve_fetch_budget()` (identical
reserve-then-rollback shape to the disk one), reserved against each call's own requested `max_bytes`
*before* the network round trip — a conservative worst-case, since the actual response size isn't known
until after the request. **CPU and RAM remain the two axes with no shortcut available**: unlike
concurrency/disk/network, there's no existing per-call counter or size argument to chokepoint against —
bounding either genuinely needs real OS-level resource monitoring (rlimits, cgroups, or platform-specific
APIs), which is real, separate, and was correctly identified as the hard part of this item from the start.

**Correction, 2026-08-30: the disk/network ceilings just above bounded one tool *instance*, not the
turn — found while auditing §2.2's `AGT-010` nested-delegation fix for the same "fresh counter per
child" shape and checking whether it also applied here.** It did: `WorkspaceTools::open_with_permissions`
builds a brand-new `Arc::new(AtomicU64::new(0))` for `bytes_written`/`fetch_bytes` every time it's
called — including every subagent child `LiveSubagentRunner::run` constructs. Since a turn can spawn up
to `MAX_SUBAGENT_SPAWNS_PER_TURN` (32) subagents (each now capped at depth 1, so this is a *bounded*
multiplier, not the unbounded shape the nested-delegation bug had), the real aggregate ceiling for one
turn was `(1 + 32) × 64 MB` disk and `(1 + 32) × 16 MB` network — 33x either constant's name, not the ~1x
"per turn" implies. **Fixed:** new `WorkspaceTools::turn_budget_handles()`/`share_turn_budgets()` — the
parent clones its own `bytes_written`/`fetch_bytes` `Arc`s and hands them to `LiveSubagentRunner`, which
now calls `share_turn_budgets` on every child's tools (right alongside `disable_nested_spawn`) instead of
letting `open_with_permissions` hand it a fresh pair. `subagent_spawns` itself needed no equivalent fix:
now that nested spawn is disabled by default, a child can never reach `execute_task_spawn` at all, so
its own fresh (and now unreachable) counter is moot. New test
`subagent_children_share_the_parents_per_turn_disk_budget`: a parent pre-loaded near the disk ceiling, a
child sharing its handles refused for hitting the *shared* budget, and a genuinely separate unshared
child's own write succeeding normally — confirming the refusal came from sharing, not from some other
cause. Full `-p rapid` suite (305 lib tests) and `cargo build --workspace --tests` pass.

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
- **Correction (2026-08-30): the premise overstates what `rapid cron` actually does — it's not "runs
  durable background prompts" yet, only "durably schedules them."** Traced `rapid cron poll`
  (`p9_commands.rs::run_cron`, `"poll"` arm) end to end: it calls `scheduler::PromptCron::poll()`, which
  does real claim-lease-firing job-lifecycle bookkeeping (due-time tracking, requeue, quarantine after
  repeated failures — all genuinely implemented, tested at the `scheduler` crate level), and the
  fired jobs are just **printed** (`id=... session=... prompt=...`) — confirmed via grep that neither
  `p9_commands.rs` nor `crates/scheduler` contains a single reference to `run_live_exec`,
  `AgentExecutionRequest`, or any other real turn-execution entry point. **A fired cron job never
  actually runs its prompt through a model or a tool call at all** — the whole background-automation
  surface this item wants to add a "propose, don't auto-apply" tier *to* doesn't exist as an executing
  system yet; there's nothing behind "poll" but a scheduler and a print statement. This is a bigger,
  more foundational gap than the tiering distinction §3.2 itself asks for, and it changes the shape of
  the real task: building execution-on-fire at all, with a deliberately read-only/propose-only mode as
  the *first* mode it supports (never a mode added after a full-execution one already shipped), rather
  than adding a tier to something that runs. Deliberately not attempted in this pass — wiring real turn
  execution into a background/unattended path carries genuine safety weight (getting a "propose-only,
  never applies" guarantee subtly wrong here is a very different risk than a CLI flag defaulting wrong)
  and deserves dedicated design attention, not a rushed pass alongside unrelated work.

### 3.3 Verified-success-per-token as a tracked, reported metric

RapidLM's own product thesis (`00-README.md`: "improve verified task success per token") is already,
independently, the same idea as Modbit's `CTX-002` Context Economy Engine ("optimize task-relevant
information per model token and verified outcome... do not sacrifice correctness for compression"). Right
now this is a stated goal with no instrumentation. Once §2.4 (tri-state completion) and §2.8 (real cost/
token accounting) both land, this becomes a computable number RapidLM can actually report per run — turning
a slogan already in the README into a real, differentiating metric neither Grok Build nor Qwen Code
publishes.

- **Sev/Effort:** P2 / S once §2.4 and §2.8 land.
- **Correction (2026-08-30): §2.4 and §2.8 have now both landed, but this item's own "then it's just S"
  claim turns out to rest on a third, previously-undocumented precondition that hasn't — checked while
  scoping this as the next tractable pick.** §2.8's cost/token accounting (`ExecOutcome.tokens`/
  `cost_usd_micros`, `apps/rapid/src/host.rs`) and §2.4's verified-completion gate
  (`CriterionEvaluator`/`GoalHost::can_complete`) are both real, but they live on two objects that never
  meet: `ExecOutcome` is per-`rapid exec`-invocation and forgotten once that process exits, while
  "verified" (a goal's evidence gate passing) is a property of a *goal*, not a single exec call — and a
  goal's own usage tracking is a fourth, separate, still-entirely-dormant subsystem. `crates/agent-runtime/
  src/goal/state.rs::GoalUsage` (`turns`/`tokens`/`active_ms`/`cost` — exactly the shape this metric
  needs) and the full `GoalBudgetGuard`/`GoalDriver` machinery that accrues and enforces it
  (`crates/agent-runtime/src/goal/driver.rs::GoalDriver::next`, `crates/agent-runtime/src/goal/budget.rs`,
  both with real, passing tests — e.g. `active_usage_accrues_and_writes_back`) have **zero call sites
  anywhere outside `agent-runtime` itself** (confirmed by grep across `apps/rapid` and every other crate) —
  the same "mature, tested, fully unwired" shape this document keeps finding elsewhere (context-engine,
  `sandbox`, now `agent-runtime`'s own goal driver). `apps/rapid/src/goal_host.rs` never references
  `GoalUsage`/`GoalDriver` at all: a goal's `usage` field stays `GoalUsage::default()` (all zeros) for its
  entire life regardless of how many real tokens/dollars `rapid exec` actually spends against it, and
  `GoalBudgetGuard`'s ceiling enforcement (the actual mechanism behind `GoalCommand::Block { budget_exhausted:
  true }`) is consequently never exercised by anything real either — a goal today can never budget-exhaust
  in production, only in `agent-runtime`'s own unit tests. **This means the metric's real blocker isn't
  §2.4/§2.8 (both done) — it's wiring per-turn usage into the active goal at all**, and `GoalDriver::next`'s
  shape (generic over the model/tool driver, i.e. designed as an orchestration loop of its own) suggests
  that isn't a small "call this after your own turn" addition: it would mean either routing `apps/rapid`'s
  existing turn loop (`interactive.rs`/`host.rs`'s fallback-chain/retry/stall-detection machinery) through
  `GoalDriver` instead, or building a narrower usage-only update path alongside it — a real design decision,
  not confirmed or attempted here. Re-scoping this item's effort to M (goal-usage wiring) → S (the metric
  itself, once usage is real) rather than the S this row currently claims.

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
