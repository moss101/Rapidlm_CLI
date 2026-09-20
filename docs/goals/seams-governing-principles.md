# SEAMS-RAPIDLM-01 — Extension seams, background work, agent control and operability: governing principles

**Goal ID:** `SEAMS-RAPIDLM-01`  
**Created:** 2026-09-19  
**Planning baseline:** `89350d7` (`fix(publication): publish from the real diff …`)  
**Status:** Phase 0 complete (2026-09-20) — see the [Phase 0 audit](seams-phase0-baseline-2026-09-20.md), ADRs 0022–0024 and [`seams-implementation-tasks.json`](seams-implementation-tasks.json)  
**Session prompt:** [`seams-goal-prompt.md`](seams-goal-prompt.md)  
**Phase 0 outputs:** [`seams-phase0-baseline-2026-09-20.md`](seams-phase0-baseline-2026-09-20.md), [`seams-implementation-tasks.json`](seams-implementation-tasks.json), ADRs [0022](../adrs/0022-hook-result-v2-decisions-rewrites-and-context.md), [0023](../adrs/0023-inbox-edges-and-subagent-delivery-modes.md), [0024](../adrs/0024-plan-proposals-approved-into-the-graph.md)  
**Normative standing:** this document sits below compiled safety invariants, `docs/PRD.md`, accepted ADRs and `docs/SDD.md`, and above `docs/tasks.md` / `docs/prompts.md`. Where it conflicts with an accepted ADR, the ADR wins and this file is corrected in the same commit. Where it conflicts with repository truth, the Phase 0 audit records the discrepancy and the item is migrated deliberately — never by silently editing this file.

---

## 0. How to use this document

1. Read it in full before starting any `SEAM-*` task. It is the contract for the whole program, not background reading.
2. §4 (the catalog) is the scope; each item's acceptance criteria are the definition of done for that item. The Phase 0 audit may **shrink, split or reorder** an item when source shows the real remaining work is different from what is written here. Only an ADR can **drop** an item or **widen** one.
3. Every "today" claim in §4 was checked against the source tree at the planning baseline by reading code or running `grep`. Recheck each one at Phase 0 with `file:line` evidence; record corrections in the audit and in the worklist, and update this file only through a commit whose message names the audit.
4. Agents are expected to cite this document by section (`SEAMS §3.2 S3`) in ADRs, delivery records and review findings, so that a decision can be traced to the rule that made it.

## 1. Purpose and the completed user experience

RapidLM already owns the hard parts: a host-owned Runtime Graph, an Event Ledger and Operation Journal, capability leases, transactional workspace views, verified orchestration and a policy lattice. What it lacks are the **seams** through which users, operators and other programs shape a run — and the **operability** surfaces that let them see and steer what is running. This program adds those seams without creating a single new authority.

When the program is complete, a user of RapidLM can:

- **Extend** the harness with hooks that can allow, deny, *ask*, defer, rewrite a tool's input, or tell the model something — every one of those decisions durable, provenance-tagged and visible, and every one narrowable by an organisation's managed policy. (`SEAM-01`, `SEAM-06`)
- **Get started in one command**: pick a provider preset, supply a key without it ever touching argv or a plaintext file, have it verified live before anything is written, and have the harness keep long answers going and retry sensibly per model. (`SEAM-02`)
- **Run long work in the background** as first-class graph nodes — demote a running command, schedule a recurring prompt, stream a monitor, reconnect and see the same live rows — and be told on resume what was still running. (`SEAM-03`)
- **Steer subagents while they run** — interject, steer or queue a message; continue a finished one; define narrow role overlays in files — with fan-out admitted by the scheduler instead of failing under load. (`SEAM-04`)
- **Plan before writing**: explore read-only, produce a diffable plan proposal, approve it, and have the approval become the graph that runs. (`SEAM-05`)
- **Govern and distribute**: sign the managed policy, allow or deny MCP servers and plugin sources before any file is written, install RapidLM's tool surface into other hosts by the shape of their config, and hand any host a skill package that teaches it to drive `rapid` headlessly. (`SEAM-06`)
- **See what it costs and where it lives**: per-turn usage from the ledger, disk usage with a reclaim plan, a scriptable status line fed by a versioned JSON payload, and an opt-in telemetry exporter that goes out only through egress policy. (`SEAM-07`)
- **Work in isolated worktrees** from headless runs, and reclaim them only when the journal proves it is safe. (`SEAM-08`)
- **Be asked less and understand more** at permission prompts, with rules that match shell scripts the way a shell reads them. (`SEAM-09`)
- **Script the CLI** with one consistent set of agent-mode flags and a structured error envelope that always names the next command. (`SEAM-10`)
- Plus the polish items in Tier 3: durable MCP elicitation and domain-bounded web search, session ergonomics, memory consolidation, a passive update notice and, last, a cross-session dashboard. (`SEAM-11` … `SEAM-15`)

Everything above is opt-in or default-preserving. A user who changes nothing sees the same RapidLM after this program as before it, except for fewer rough edges in error messages.

## 2. Naming, provenance and clean-room rules

These rules are absolute for this program and are checked by the reviewer on every diff.

- **2.1 No peer products are named — anywhere.** Code, identifiers, comments, tests, fixtures, docs, ADRs, delivery records, worklist entries, commit messages and PR text produced under this program never name another coding-agent product, CLI, IDE or their vendors' agent products. Say *peer tools*, *external hosts*, *reference implementations*. Describe a feature by what it does, never by where it was seen.
- **2.2 Upstream model providers** may be named only where the wire protocol or endpoint requires it: inside the existing provider modules under `crates/llm-router/src/providers/` and inside the preset table `SEAM-02` introduces (as data rows and their tests). Not in prose, headings, commit messages or comments beyond the identifier itself.
- **2.3 Clean room.** This program adapts *designs*, not code. No source, prompt text, fixture or test is copied from any external project. If a future task ever needs to reuse external code, it stops and gets an explicit decision plus a license/NOTICE review first; the default is that it does not happen.
- **2.4 Clean what you touch, do not purge the repository.** When a task edits a file that already contains a peer-product name (some historical documents and one reference heading do), it removes that name in the same change. No task under this program performs a repository-wide purge of historical documents; that is a separate decision for the user.
- **2.5 Host discovery is by shape, not by name.** Any feature that must locate another program's configuration (`SEAM-06` installer) finds it by the *shape* of the file (a servers map under a small set of known key names, in the standard home/XDG configuration roots) and reports it by path. Nothing in the codebase maps a product name to a path.
- **2.6 Mechanical enforcement is deliberately absent.** A deny-list of names would itself violate 2.1. Enforcement is the reviewer agent's checklist (§5.5) and the self-review pass every commit already gets.

## 3. Governing invariants

### 3.1 Core invariants

The fifteen core V3 invariants in [`00-README.md`](../../00-README.md) apply unchanged. The ones this program leans on hardest, with the items that depend on them:

| # | Invariant (abridged) | Items |
|---|---|---|
| 1 | Graph state is host-owned; models submit proposals | 03, 04, 05 |
| 4 | No acknowledged durable transition precedes Event Ledger append | 01, 03, 04, 07 |
| 5 | Every side effect is policy-classified and lease-enforced | 01, 02, 06, 09, 11 |
| 6 | Uncertain external effects are reconciled, never blindly replayed | 02, 08 |
| 7 | Parallel write-capable agents never share a mutable `WorkspaceView` | 04, 08 |
| 10 | Tool outputs and external content are bounded, provenance-tagged, untrusted | 01, 03, 06, 11 |
| 11 | Secrets remain handles until the executor boundary | 02, 06 |
| 12 | Subagents receive minimal typed envelopes, not the parent transcript | 04, 05, 12 |
| 13 | Hooks, skills, MCP, plugins can request capabilities but never grant them | 01, 04, 06, 09 |
| 14 | Autonomous goals recover as paused after restart | 01, 03, 05, 11 |
| 15 | UI is a projection of kernel state, not a source of truth | 03, 07, 15 |

### 3.2 Program invariants

- **S1 — Everything is a node, an event or a gate.** A new behaviour is expressed as a graph node type, a ledger event, or a policy gate. Never as a side channel (an in-memory registry that is the only record, a prompt-only restriction, a TTY-only prompt).
- **S2 — Nothing prompts on a TTY that could not survive a restart.** A hook `ask`, an elicitation, a plan approval, a permission prompt from a background child: each is a Human/Approval wait on the graph, recorded in the ledger, resumable. Headless runs answer such waits with the documented "needs a human" exit code, never with an implicit allow.
- **S3 — No silent rewrites.** Any change to what a tool receives or what a model sees that did not come from the user or the model (a hook's rewritten input, injected context, a continuation stitch) carries provenance — source id, digests of before/after, the event that applied it — in the journal, and leaves a one-line marker in the transcript.
- **S4 — External content stays outside the trust boundary.** Hook `additional_context`, monitor lines, search results, MCP structured content and anything read from another host's configuration are bounded, fenced and provenance-tagged like tool output. Instructions found inside them are data.
- **S5 — Policy only narrows.** Role overlays, presets, installers, hooks, startup grants and file-based configuration can request; the lattice and managed policy decide; a lower-trust layer can only ever restrict what a higher one allows. Managed gates report field, origin and remediation, as the existing gates do.
- **S6 — Projections, not authorities.** `/jobs`, the status line, `rapid usage`, `rapid du`, the resume summary and the dashboard are *derived* from the ledger, journal and graph. If a derived view and the ledger disagree, the ledger is right and the view has a bug.
- **S7 — Wire before you build.** Spend the first fifteen minutes of every task grepping for the primitive. The pattern documented in `newtask.md` §0a — mature, tested crates that are simply not reached from `apps/rapid` — recurred in every previous programme. The likely task is "reach crate X from the exec loop", not "build X".
- **S8 — Verified before trusted.** A fix is trusted only after its test has been reverted-and-confirmed-failing against the pre-fix code and then reinstated green. A weakened assertion is a regression, not a fix. Every feature/fix commit gets a background adversarial self-review before the next task starts. Never run two `cargo` suites concurrently (concurrent runs fabricate failures). Windows is a test gate: a new contract ships with portable tests or a typed-unavailability test for its unsupported path.
- **S9 — Truthful surfaces.** `doctor`, `usage`, `du`, the status line and every error say what was actually measured, and distinguish *reported* from *estimated*, *unsupported* from *failed*, *unchanged* from *skipped*. "No files were changed" is stated explicitly on every failed onboarding or installation.
- **S10 — Egress only through policy.** The live probe, the update check, the telemetry exporter, web search and any external-host handshake go out through the capability broker with an egress receipt, are opt-in where the feature is new, and never run in CI or non-TTY contexts unless a flag says so.
- **S11 — Defaults preserve today.** Configuration precedence stays managed > CLI > env > user > workspace > built-in. New modes are opt-in. A file that is not touched is byte-identical afterwards. A user who changes nothing gets the same behaviour, the same exit codes and the same JSONL.

## 4. The catalog

Each item states its intent, what exists **today** (verified at the baseline), the design in RapidLM's own terms, acceptance criteria (`AC-nn`, each testable), dependencies, and the invariants it touches. Effort is deliberately not estimated here: the audit establishes it from source (S7).

### Tier 1 — the extension seams everything else builds on

#### SEAM-01 — Hook decision contract v2

**Intent.** Hooks become a real policy seam: allow, deny, *ask*, defer; rewrite a tool's input; tell the model something — with none of it becoming a silent side channel, and all of it narrowable by managed policy.

**Today.** `apps/rapid/src/hooks.rs`: `pre_tool_use` command hooks receive the tool call as JSON on stdin; a non-zero exit denies with stderr as the model-visible detail; a hung hook denies; `post_tool_use` output is recorded on the result. `crates/plugin-host/src/hooks.rs`: `HookEvent` covers SessionStart/SessionEnd/TurnStart/TurnEnd/Stop/Notification; `HookDecision { Continue, Block }`, `HookDisposition { Continue, Warn, Block }`, `HookKind { Command, Http, Plugin }`; each hook declares requested capabilities and a sandbox profile. `apps/rapid/src/managed_config.rs` has no hook gate.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** the production hooks have eight stages (`pre_tool_use, post_tool_use, session_start, session_end, subagent_start, subagent_stop, pre_compact, post_compact`) and `run_hook_once` merges stdout and stderr into one file, so no structured result channel exists yet; `plugin_host::HookEvent` has 27 variants (no `Stop`/`Notification`) and that engine has zero production callers (`rapid plugins hook-test` is a dry-run); there is no hooks reference page to update — AC-08 creates `docs/reference/hooks.md`; the durable approval wait already exists (`approvals.rs`) and headless `rapid exec` has no approval sink; `crates/tool-gateway` is not reachable from `apps/rapid`, so "re-validated against the tool contract" means the per-tool argument parsers. Design settled in ADR 0022.

**Design.**
- A versioned hook result on stdout (`rapidlm.hook_result`, schema v2): `decision ∈ {allow, deny, ask, defer}`, `reason`, `updated_input` (object), `additional_context` (string), `hook_specific` (object). v1 hooks (exit-code only) keep working unchanged; absence of JSON means v1 semantics.
- `ask` creates a Human/Approval wait on the graph — the same `waiting` state the workflow runner's human steps use — recorded in the ledger before the turn pauses; resolution resumes the turn; a restart mid-`ask` recovers paused (S2, invariant 14). Headless: the documented "needs a human" exit code, never an implicit allow.
- `defer` states no opinion: the call takes the normal permission flow; recorded.
- `updated_input` is re-validated against the tool contract (`crates/tool-gateway` validation); failure denies with the hook named. Original and rewritten inputs, hook id and content digests are journaled and a one-line transcript marker names the hook (S3). When several hooks rewrite, the last wins; a `deny` discards all rewrites.
- `additional_context` is delivered *after* the tool result as a fenced, bounded, provenance-tagged untrusted block naming the hook (S4).
- New events: `UserPromptSubmit` (may block with a reason; queued prompts hold), `StopCancelled` (fires *instead of* `Stop` when a turn ends without completing — interrupt, denied permission, budget, max turns — and carries the reason), `SubagentStop` (in the child; may block the stop), `PostToolUseFailure` (dispatch failure or MCP error result; may add context, cannot block).
- Successful hooks are silent; only a blocking or failing hook shows one transcript line.
- Failure semantics stay as today: a pre-tool hook that times out, crashes or emits malformed output denies; other events fail open with a recorded warning.
- Managed policy: `hooks.managed_only` (only hooks from the managed policy run) and `hooks.denied_events`; narrow-only, reported like the existing gates.

**Acceptance.**
- AC-01 Every existing hook test passes unchanged; a v1 exit-code hook behaves byte-identically.
- AC-02 `allow`, `deny`, `ask`, `defer` each have a test proving their effect on dispatch and their ledger record.
- AC-03 `ask` resolves through the approval surface in the TUI; in headless it produces the documented exit code; a process restart during `ask` resumes as paused/waiting with the call not re-run.
- AC-04 A rewrite failing the tool schema is denied naming the hook; a valid rewrite runs the rewritten call, journals both inputs with digests, and the transcript marker is present.
- AC-05 `additional_context` appears after the tool result, fenced, naming the hook, bounded by the existing tool-output ceiling.
- AC-06 The four new events fire in the documented situations with the documented payloads; `Stop` does not fire when `StopCancelled` does.
- AC-07 `hooks.managed_only` blocks non-managed hooks and reports field/origin/remediation.
- AC-08 The hooks reference page is updated and the v2 result schema has a fixture under `crates/protocol`'s schema-fixture test.

**Depends on:** nothing. **Invariants:** 4, 5, 10, 13, 14; S1–S5.

#### SEAM-02 — Provider onboarding and model-call resilience

**Intent.** One command takes a new user from nothing to a verified, safely stored model configuration; and the router keeps long answers going and retries per model, so smaller and local models become more useful without prompt tricks.

**Today.** Configuration is hand-written (`docs/reference/model-configuration.md`: `[models]`, `[model.<id>]` with `provider`, `model`, `base_url`, `env_key`). Two wire dialects exist under `crates/llm-router/src/providers/`. `crates/auth` provides OS and file keychains and `CredentialKind::{ProviderApiKey, OauthRefresh, OauthAccess}`; `crates/llm-router/src/credentials.rs` resolves them; `crates/auth/src/mtls.rs` exists. `rapid doctor` is offline by design and reports "unconfigured" on a fresh machine. Retry is one global knob (`RAPIDLM_RETRY_BASE_MS`). The catalog carries prices, regions and latency classes. There are no presets, no live probe, no continuation on length-truncated output, no per-effort model identifiers, no per-model retry policy, no reasoning-summary setting.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** `[model.<id>]` accepts twelve keys (the page documents eight and omits `[models] fallback` and `[phases]`); the keychain is never used for provider credentials (`resolve_credential` is inline `api_key` > `env_key` > keyless into an in-memory store); `doctor` reports `WARN "no model configured"` and exits 0 (the getting-started page says 1) and accepts no arguments; `RAPIDLM_RETRY_BASE_MS` lives in `apps/rapid/src/host.rs` and there are two retry layers (step supervision, fallback chain); the provider transport dials directly with no `EgressProxy`, no proxy-variable support and no receipt, while `journal::EgressReceipt` exists with no producer; the ledger's `model.completed` carries a token total only; `FinishReason::Length` is produced by both adapters and dropped in `model.rs::fold_stream`.

**Design.**
- `rapid setup [--preset <id>] [--profile <id>] [--model <id>] [--base-url <url>] [--key-env <VAR> | --key-stdin] [--dry-run] [--non-interactive] [--output json] [--no-verify]`. Interactive wizard on a TTY; every option makes it scriptable. Presets are a data table in code (endpoint, dialect, default model, documentation link, local-server flag) with tests; rule 2.2 applies.
- Keys arrive via an environment variable or stdin only — never argv — and are stored in the keychain as `ProviderApiKey`; the config references the handle (`env_key` or a keychain reference), never the value (invariant 11).
- **Live verification before persisting**: one minimal request (≤16 output tokens) issued through the router itself, classified as auth (401/403), quota (402/429), network (DNS, timeout, proxy — proxy environment variables are recognised), server (5xx) or invalid response; each maps to a distinct exit code and a hint; the message states "no files were changed"; the probe is egress under policy with a receipt (S10).
- Persistence: temp-file + rename, mode 0600, a timestamped `.bak` only when a previous file changes, and an idempotent `unchanged` outcome.
- `rapid doctor --live` runs the same probe for every configured profile; the offline `doctor` is unchanged.
- `[model.<id>]` extensions: `effort_ids = { low = "…", medium = "…", high = "…" }` (resolved at route time; default is `model`); `retry = { max_attempts, base_ms, max_ms, on = ["rate_limit", "server", "network"] }` overriding the global; `reasoning_summary` for dialects that support it; `continue_on_length = N` — when a response ends for length, up to `N` continuation requests carry the partial output forward, the stitched result is one assistant message, and each continuation is a ledger event (`continuation_of`, index) that counts against budgets (S3). Retry and continuation status appear with a short reason in exec output and the TUI.
- Every error from this item carries a hint naming the next command (`rapid setup`, `rapid doctor --live`).

**Acceptance.**
- AC-01 `rapid setup --dry-run --non-interactive --output json` prints exactly the files and keys it would write and performs no network call and no disk write.
- AC-02 A failed verification leaves the configuration directory and keychain byte-identical (hashed before/after) and exits with the auth/quota/network code and hint.
- AC-03 A successful setup writes the config atomically at 0600, stores the key in the keychain, writes `.bak` only when a previous file existed, and a second identical run reports `unchanged`.
- AC-04 `effort_ids` selects the per-effort identifier in the outgoing request for each dialect.
- AC-05 Per-model `retry` overrides the global policy; retryable classes are tested; non-retryable errors are never retried.
- AC-06 With `continue_on_length = 2`, a stubbed provider returning length-truncation twice then completion yields one stitched message, three ledger events with `continuation_of`, and three budget decrements; `= 0` preserves today's behaviour exactly.
- AC-07 `doctor --live` reports one typed check per profile; offline `doctor` output is unchanged.
- AC-08 `docs/reference/model-configuration.md` and `docs/getting-started.md` are updated, and the reference page's heading no longer names a peer product (rule 2.4).

**Depends on:** nothing. **Invariants:** 5, 6, 11; S3, S9, S10, S11.

#### SEAM-03 — Background work as first-class graph nodes

**Intent.** Long-running work is a node, not a special case: demote a running command, schedule a recurring prompt, stream a monitor, reconnect and see the same rows, and be told on resume what was still running.

**Today.** `shell_exec` supports background/detached execution (`apps/rapid/src/exec_tools.rs`); the TUI has a `/jobs` panel and a job registry (`crates/tui/src/state.rs`, `commands.rs`); `crates/process-supervisor` and `crates/process-signal` exist; the scheduler has cron leases (`crates/scheduler/src/cron.rs`) and wake-on-event (ADR 0015). There is no `/loop`, no demote keybinding, no monitor tool, and no host-generated resume summary of still-running work. Whether `/jobs` rows are already derived from ledger events is to be verified at Phase 0.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** **verified: `/jobs` rows are derived from `job.*` ledger events** (`AppState::apply_kernel`), so the item keeps and proves that; `/jobs logs|cancel` go to the in-process registry. Gaps the audit found instead: daemon/ACP turns build a fresh `JobRegistry` per turn (jobs die at turn end there, unlike the TUI), a detached `task_spawn` never emits `finished`, `job.output`/`job.orphan_reconciled` have no producer and no reconciliation runs at restart, ACP `session/update` drops `job.*`, `rapid exec` JSONL emits four record types and no job events, notifications are an in-memory vector. `rapid cron add|list|remove|poll` over `scheduler::PromptCron` exists (a one-shot tick, fired prompts run as Plan-mode turns) — the `/loop` substrate. ADR 0015 names no types; the only wake code (`HostRuntime`) is unconstructed; `process_supervisor::{MonitorSpec, TriggerSpec, reconcile_orphans}` are unwired.

**Design.**
- **Background node.** A running Process node can be demoted (TUI keybinding and `/jobs bg <id>`): the turn stops waiting, the process keeps running, completion wakes the session through the existing wake-on-event path. Live rows show a bounded, tail-kept stream (the existing log rule).
- **Durable job projection.** `/jobs` and daemon/ACP clients rebuild rows from ledger events on reconnect; no in-memory registry is the only record (S6, invariant 15). If the registry is already event-derived, keep it and prove it.
- **`/loop <interval> <prompt>`** and `rapid loop add|list|rm`: a cron lease scheduling a *background* Agent node with its own bounded context; results land as notifications and ledger events, never in the foreground transcript; default lifetime 7 days; at most 50 active loops; each visible in `/jobs` with next fire and expiry; deletable from the panel.
- **`monitor` tool.** An event-driven monitor node runs a command and turns each stdout line into a bounded, provenance-tagged notification (S4); flood control stops it above a rate with a notice and a hint to restart with a tighter filter; `persistent: true` lives for the session; the existing kill path stops it.
- **Resume summary.** On `/resume` and `rapid exec --continue`, the model receives a short host-generated block derived from the graph's non-terminal nodes — background processes, monitors, loops, subagents, workflow runs. Derived on demand, never stored separately (S6).
- **Waits.** Output/completion waits get a configurable ceiling (default one hour); a timeout is reported as "still running", not failure.

**Acceptance.**
- AC-01 Demoting a running command keeps it alive, ends the turn, and completion wakes the session; the ledger shows the node's transition to the background/waiting state.
- AC-02 Killing the client and reconnecting through the daemon rebuilds identical `/jobs` rows from the ledger alone.
- AC-03 A loop fires on schedule in a background node; its output never enters the foreground transcript; it expires after the configured lifetime; the cap refuses the 51st with a typed error.
- AC-04 `monitor` emits one notification per line with provenance; a flood auto-stops with a notice; a persistent monitor survives turn boundaries and stops on kill.
- AC-05 Resume after a restart with a background process alive yields the derived "still running" block; with nothing running, no block.
- AC-06 `rapid exec` JSONL carries the same job events as the TUI.

**Depends on:** nothing hard (`SEAM-01`'s `StopCancelled` is used for cancelled waits when present). **Invariants:** 1, 4, 14, 15; S1, S6.

#### SEAM-04 — Subagent conversation control

**Intent.** A running subagent can be spoken to — interjected, steered or queued — a finished one can be continued, roles can be narrowed in files, and fan-out is admitted by the scheduler rather than failing under load.

**Today.** `crates/agent-pool`, `crates/handoff`, `crates/agent-runtime` (`role_profile.rs`/`specialist.rs` — a code-defined `RoleRegistry`), typed task/context envelopes, `/agents spawn|integrate|abandon` (`apps/rapid/src/agent_views.rs`), managed `max_subagent_spawns_per_turn`. No message delivery to a running child, no continuation of a finished child, no file-based role overlays, no admission queue (verify).

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** `crates/agent-pool` is a warm *environment* pool and `crates/handoff` is foreground→daemon ownership transfer — neither is a subagent primitive and neither has a production caller; there is **no `/agents spawn`** (spawning is the model-invoked `task_spawn` tool with `type ∈ {general-purpose, explore, plan}`, and the child is always `AgentRole::Coder`); file-based role overlays **exist** (`agent_runtime::agent_defs`, `.rapidlm/agents/*.toml`, narrow-only, via `rapid agents list|validate|scaffold`) but the spawn path never reads them; the detached ceiling (4) rejects rather than queues; `agent.mail.sent/dropped` event kinds exist with no emitter; `PersistentSpecialist` has an unwired mailbox. Design settled in ADR 0023.

**Design.**
- **Inbox edges.** A typed message edge into an Agent node with `delivery ∈ {interject, steer, queue}`: *interject* pre-empts the node's current wait and is read immediately; *steer* applies at the next turn boundary; *queue* is delivered after the current run completes. Each is a ledger event; the transcript labels the mode.
- **Continue a finished child.** A message to a terminal Agent node appends a new turn in the node's lineage (a superseding revision, the same mechanics as fork/rewind — invariant 3) instead of failing.
- **Admission.** Spawns pass through the scheduler with a concurrency ceiling and a queue; wide fan-outs wait in order rather than exhausting descriptors; ceilings are narrow-only from managed policy.
- **Role overlays.** `.rapidlm/agents/*.toml` (project, trust-gated) and `~/.rapidlm/agents/*.toml` (user): `name`, `description`, `base_role`, `instructions`, `model`, `reasoning_effort`, declared `inputs` and `outputs`. An overlay can only narrow its base role's tool surface (invariant 13, S5); it appears in `/agents`; the parent reads declared inputs/outputs to compose envelopes (invariant 12).
- **Toolset from role only.** No per-spawn parameter can widen capability; if one exists it becomes narrow-only.
- **Wait ceiling.** Subagent waits default to one hour; timeout means "still running".

**Acceptance.**
- AC-01 Each delivery mode has a test proving *when* the child sees the message (during a wait / at the next turn / after completion), its ledger event and its transcript label.
- AC-02 Messaging a completed child continues it in the same lineage; the graph shows the superseding revision.
- AC-03 Spawning twice the ceiling queues in order rather than erroring; a managed ceiling cannot be exceeded by user or project configuration.
- AC-04 An overlay requesting a tool outside its base role is rejected at load with field-level remediation; a valid overlay appears in `/agents` and applies its model and effort.
- AC-05 A declared output missing at completion is a typed integration failure, never a silent acceptance.

**Depends on:** `SEAM-03` (background waits). **Invariants:** 1, 3, 7, 12, 13; S1, S5.

#### SEAM-05 — Plan mode as a graph proposal

**Intent.** Explore read-only, produce a diffable plan, approve it, and let the approval become the graph that runs — with the read-only guarantee enforced by policy, not by prompt.

**Today.** No `/plan`. The scheduler has a proposal type (`crates/scheduler/src/proposal.rs`); the playbook compiler turns steps into nodes; tools declare `read_only` (`crates/tool-gateway/src/schema.rs`); human steps wait on the graph.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** plan mode exists twice — `plan_enter`/`plan_exit` tools with a `.rapidlm/plan.md` carve-out (an in-memory `ExecTools` flag; `plan_exit` reports "Plan accepted" with no human decision) and `PermissionMode::Plan`, which denies every non-read-only class at the lattice before any rule (AC-01's guarantee already exists in policy); `crates/tool-gateway` has no per-tool `read_only` field and no dependent — the exec loop's classification is `tool_kind`/`tool_class` in `exec_tools.rs`; human steps wait through the workflow runner (`StepState::WaitingHuman`), and `GraphService::wait` is reached only via `rapid run --orchestration verified`. Design settled in ADR 0024.

**Design.**
- `/plan` in the TUI and `rapid exec --plan`: the turn runs an Agent node whose reachable toolset is read-only — enforced by the permission lattice — and produces a plan proposal (`rapidlm.plan_proposal` schema: steps, files expected to change, verification steps, risks, open questions) stored as an artifact and rendered as a plan file under `.rapidlm/plans/<id>.md`.
- An Approval node gates scheduling of any write-capable node. Approval compiles the proposal into graph nodes with `DependsOn` edges (the playbook path). Editing the plan creates a new revision; the old one is immutable (invariant 3). Rejection records the reason.
- Plan mode is sticky until approved or cancelled; a write-capable call in plan mode is a typed refusal the model can read.

**Acceptance.**
- AC-01 In plan mode a write tool call is refused by the lattice (not by prompt) — tested at the policy layer.
- AC-02 The proposal artifact validates against its schema fixture.
- AC-03 Approval yields graph nodes whose edges match the plan, and a ledger record links plan revision → nodes.
- AC-04 Editing before approval creates a new revision and leaves the previous one intact.
- AC-05 Headless `--plan` prints the proposal and exits with the "needs a human" code.

**Depends on:** `SEAM-01` (`ask`/approval path). **Invariants:** 1, 3, 5, 8; S1, S2.

### Tier 2 — distribution, governance, operability

#### SEAM-06 — Governance and distribution

**Intent.** Managed policy is signed and decides *before* anything is written; MCP servers and plugin sources are allow/deny-listed; tool-name collisions resolve deterministically; RapidLM's tool surface can be installed into other hosts by the shape of their configuration; and any host can be handed a skill package that teaches it to drive `rapid` headlessly.

**Today.** `RAPIDLM_MANAGED_CONFIG` points at an unsigned policy with the gates listed in `apps/rapid/src/managed_config.rs`; no MCP or plugin-source allow/deny list. `rapid mcp list|get|add|remove|probe` and `mcp-tools` exist; `apps/rapid/src/mcp_config.rs` reads project settings (including one compatibility path for a peer host's project settings file — leave it, do not extend it). `crates/security` observes release signatures (`ReleaseSignatureObservation`). No installer for external hosts, no skill package, collision precedence and User-Agent presence to be verified.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** `ReleaseSignatureObservation` is an observation enum; **the workspace has no asymmetric-signature primitive** (only SHA-256 digests — `rapid update` verifies a digest and never parses the manifest's documented `signatures`), so the signed-policy design needs a dependency decision (audit §5 D-1); `mcp add` writes with no trust gate (trust is an advisory note afterwards); MCP tools are `mcp__<server>__<tool>` with prefix dispatch after exact built-in matches and no name dedup, and there are no plugin tools in the exec toolset; `User-Agent` is sent only by `http_get` (`rapidlm-web-fetch`), not by the provider POST writer or the MCP HTTP transport; `PluginInstaller` exists with no caller (`rapid plugins register` writes the trust store only).

**Design.**
- **Signed policy.** The managed policy may carry a detached signature; `managed.require_signature = true` refuses an unsigned or invalid policy fail-closed using the same verification primitives as release signatures; `doctor` shows policy origin and signature state.
- **Allow/deny lists.** `managed.mcp.allowed_servers` / `denied_servers` (by name and by command/URL pattern) and `managed.plugins.allowed_sources`, enforced at `mcp add`, plugin install and session bind — before any file is written — with the existing field/origin/remediation report; `mcp probe` and `doctor` list what policy blocks.
- **Collision rule.** Built-in > plugin > MCP; MCP tools are reachable as `mcp__<server>__<tool>`; a collision is reported once, deterministically.
- **User-Agent.** `rapid/<version>` on MCP HTTP transports and provider calls, if absent today.
- **Installer by shape.** `rapid mcp install --into <path> [--format jsonc|toml|yaml] [--key <servers.key.path>] [--dry-run]` and `rapid mcp install --discover`, which scans the home/XDG configuration roots for files whose shape declares MCP servers and lists them by path (rule 2.5). Edits are format-preserving (comments, ordering and unrelated keys untouched), configuration roots are overridable by environment, a timestamped `.bak` is written, the write is atomic at 0600, a per-user lock prevents concurrent runs, a second run is `unchanged`, and dry-run prints the diff.
- **Skill package.** `skill/SKILL.md` in the open skill format teaches an external host to drive `rapid` headlessly: agent-mode flags, the stdout/stderr contract, exit codes, JSONL, resume rules ("wait on the exact session; never start a second goal because a resume was interrupted"), key-handling rules. A sub-skill covers long-running goals.

**Acceptance.**
- AC-01 An unsigned policy under `require_signature` is refused fail-closed before a session starts, with remediation.
- AC-02 `mcp add` of a denied server writes nothing and reports the gate; a denied plugin source likewise.
- AC-03 A tool present both built-in and via MCP resolves to the built-in and remains reachable under the namespace.
- AC-04 The installer on fixture files preserves comments, ordering and unrelated keys (the byte diff is limited to the injected block), writes `.bak`, is `unchanged` on the second run, and writes nothing under `--dry-run`.
- AC-05 `--discover` finds fixture files by shape only; fixture names are neutral.
- AC-06 A test executes every command in the skill package against `--help` so that no documented command or flag can drift.

**Depends on:** nothing. **Invariants:** 5, 13; S4, S5, S10.

#### SEAM-07 — Usage, disk and status-line observability

**Intent.** Users can see what a session cost, where RapidLM's data lives and how much of it is reclaimable, and can put live session facts on a scriptable status line — all derived from the ledger.

**Today.** `crates/llm-router/src/usage.rs` (`UsageAccumulator`, `AccountedCost`, `ModelUsage`); goal budgets and usage in the goal snapshot; `rapid insights`, `rapid inspect-export`; `crates/tui/src/status.rs` renders a fixed `StatusLine` (connectivity, context usage, policy mode, sandbox mode); `crates/telemetry` has OTLP-shaped records, an `ExporterPolicy` and redaction with a local, never-sent bundle. No `rapid usage`, no `rapid du`, no configurable status line, no network exporter.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** the ledger records **no cost** (`model.completed` has `tokens: u64` only; cost lives in-process and reaches `--usage-file`, the goal file and JSONL), so a ledger-derived `usage` needs the payload extended first; `/context` already exists with a used/cap view model; no crate depends on `crates/telemetry` and its only `OtlpTransport` impls are test doubles; `RAPIDLM_HOME` holds `config.toml`, `project-trust.json` and `project-permissions.json` only — sessions, index, findings, runs and plans are per-project under `.rapidlm/`, and there is no caches directory, so `du` measures the real layout.

**Design.**
- `rapid usage [<session>] [--project] [--since <duration>] [--output json] [--quiet]`: per-turn tokens, cost and model from the ledger; totals; TSV under `--quiet`; *reported* vs *estimated* cost flagged (S9).
- TUI `/usage` and `/context` in one tabbed modal; the context legend sums to 100%.
- `rapid du [--reclaim-plan]`: sizes of sessions, worktrees, the artifact store and caches under `RAPIDLM_HOME`; reclaimable entries per `SEAM-08`'s rules; never deletes.
- `[ui.status_line]`: `type = builtin | command | disabled`, `items`, `command`, `refresh_interval` (1–86400 s). The script receives a schema-versioned JSON payload on stdin (`session_id`, `model`, `effort`, `context.used_percent`, `cost`, `goal.state`, `worktree`, `workspace.cwd`/`repo`, `trigger = state | refresh_interval`); runs under the sandbox ladder with a timeout; up to five lines cut at 1024 characters; the last output is retained when a refresh fails; a project-level status command requires project trust.
- Telemetry: opt-in `[telemetry.otlp] endpoint`; egress under policy with a receipt; redaction first; bounded queue; loss is never a caller-visible failure.

**Acceptance.**
- AC-01 `usage` totals equal the sum of ledger records for a fixture session; estimated vs reported is flagged.
- AC-02 `du` matches the filesystem for a fixture home within block rounding; `--reclaim-plan` lists only entries `SEAM-08` would reclaim.
- AC-03 The status command runs sandboxed, times out to the last output, honours `refresh_interval`, its payload validates against the schema fixture, and an untrusted project's status command does not run.
- AC-04 The exporter is disabled by default, sends only redacted records, and records an egress receipt.

**Depends on:** nothing. **Invariants:** 10, 11, 15; S6, S9, S10.

#### SEAM-08 — Worktree lifecycle

**Intent.** Headless runs and goals can run in isolated worktrees; worktrees are reclaimed only when the journal proves it is safe; a matching local checkout is reused instead of re-fetched.

**Today.** `crates/workspace/src/backends/git_worktree.rs`; `/fork`; `/agents spawn` creates child worktrees; `TransactionManager` publication. No `--worktree` on `exec`/`goal`, no reclaim, no disk accounting.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** `/fork` forks the ledger session and creates no worktree; write-capable `task_spawn` children get a view through `AgentViewManager::create_for`, whose registry is an in-memory map lost on restart; worktrees live under the repository's `.git/rapidlm/worktrees`, not `RAPIDLM_HOME/worktrees/…` (audit §5 D-2, recommendation: keep the in-repo location); `abandon` is not journaled; `PersistedWorktree` carries a write owner but no session or lease id; `TransactionManager` has `Staged|Committed|RolledBack` and no `abandon` verb.

**Design.**
- `rapid exec --worktree [<name>]` and `rapid goal create --worktree`; worktrees live under `RAPIDLM_HOME/worktrees/<project-id>/<name>`.
- Reclaim rule: a worktree is reclaimable only when its `WorkspaceView` transaction is published or abandoned in the Operation Journal, no active lease or session references it, and it has no unpublished changes. `rapid worktree list|reclaim [--dry-run]`. The project's primary checkout is never a candidate.
- A matching local checkout is reused as the base for a linked worktree; branch-tip fetch is the default, `--full-history` opt-in.

**Acceptance.**
- AC-01 Headless `--worktree` runs in a separate worktree and leaves the primary tree's `git status` unchanged.
- AC-02 Reclaim refuses a worktree with unpublished changes or an active lease; `--dry-run` lists without deleting.
- AC-03 A published worktree is reclaimed and the journal records it.
- AC-04 Base reuse produces a linked worktree without network access.

**Depends on:** `SEAM-07` (soft, for `du`). **Invariants:** 6, 7; S6, S9.

#### SEAM-09 — Permission UX

**Intent.** Fewer prompts, better prompts: persisted allow/deny answers, an AST-aware shell matcher, full context at the prompt, and bounded startup grants for CI.

**Today.** `RAPIDLM_PERMISSION_MODE`; `rapid permissions list|allow|revoke` (`apps/rapid/src/permissions_cli.rs`); deny rules exist in `apps/rapid/src/permissions.rs`; managed `max_permission_mode` and `denied_tools`; approvals in `apps/rapid/src/approvals.rs`.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** shell rules match `argv.join(" ")` with `*`/`?` glob only; a bounded shell tokenizer (`security::tokenize_shell`) exists and is advisory text only; "approve and remember" already persists allow grants for non-shell tools and there is no persisted never-allow; `permissions list` shows persisted grants without origin; `Auto` equals `AcceptEdits` in the mode table; the production approval modal is three lines while a rich `tui::ApprovalViewModel` (diff, script) is exported, snapshot-tested and unused by production.

**Design.**
- "Never allow" and "always allow" offered at prompts for MCP tools and fetch domains, persisted per project, shown with origin in `permissions list`.
- An AST-aware shell matcher (quoted variables, pipelines, `;`/`&&`, subshells, heredocs) so quoting neither bypasses a rule nor over-prompts; unknown constructs prompt (conservative).
- A safe-command list (directory creation, touch, listing) in auto mode.
- Prompts render the full script and auto-expand the file-edit diff.
- `rapid exec --allow <rule>` (repeatable) as startup grants bounded by the managed ceiling; the default mode is configurable in user config.

**Acceptance.**
- AC-01 A persisted "never allow" blocks without prompting on the next run and appears in `permissions list` with its origin.
- AC-02 Matcher fixtures for each construct produce the expected decision.
- AC-03 `--allow` cannot exceed the managed ceiling (gate test).
- AC-04 The TUI approval renders the diff and full script (snapshot test).

**Depends on:** `SEAM-01`. **Invariants:** 5; S5, S11.

### Tier 3 — polish

#### SEAM-10 — Agent-mode flags and error envelope

One global flag set parsed once — `--output json|text`, `--quiet`, `--non-interactive`, `--dry-run`, `--yes`, `--no-color`, `--timeout` — with environment mirrors (`RAPIDLM_OUTPUT`, …); non-TTY stdout defaults to JSON for commands that have a JSON form; a typed CLI error (`code`, `message`, `hint`) rendered as `{"error": {…}}` on stderr in JSON mode; every `InteractiveError` gains a hint naming the next command; headless sessions are tagged in the ledger (`origin = headless`) so `sessions list` and the resume picker can filter them. **AC:** a table-driven test shows the flags honoured by every subcommand; the envelope has a schema fixture; exit codes are unchanged; `sessions list --origin` filters. **Depends on:** nothing.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** none of the seven flags exists today (each subcommand parses its own; `doctor` rejects `--json`); errors are `eprintln!` + exit code with no envelope or hint; `session.created` carries `{ project_id }` only; `rapid sessions list|search` filters by id substring.

#### SEAM-11 — MCP elicitation and web search

Multi-round-trip elicitation (a tool call returns an input-required state with a schema; the host asks through the Human/Approval node — durable; the retry carries the answer) and MCP form/URL-consent requests through the same node; a `web_search` tool behind a backend trait with `[toolset.web_search] backend, allowed_domains, excluded_domains`, results bounded and provenance-tagged, egress under policy. **AC:** an elicitation round-trip against a stub server; a restart mid-elicitation resumes; results filtered by the domain lists; no backend configured is typed unavailability. **Depends on:** `SEAM-01`.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** the MCP client declares only `roots` and discards server-initiated frames; `web_fetch` has SSRF classification, an exact-host allowlist and a 100 KB cap but no ledger record, receipt or lattice check, and its success summary is cut to 256 bytes by `bounded_detail`.

#### SEAM-12 — Session ergonomics

`RAPIDLM_SESSION_ID` and `RAPIDLM_TURN_ID` exported to tool commands, hooks, MCP servers and status scripts; `/aside <question>` — a detached read-only Agent node whose answer is shown but never enters the main context; a host-generated `/resume` recap (last-turn summary, durations, unmet criteria); `/rename [--auto]`; a prompt stash keybinding; `/edit-prompt` in `$EDITOR`. **AC:** one test per item; the aside's content is provably absent from the next turn's compiled context. **Depends on:** `SEAM-03`, `SEAM-01`.

#### SEAM-13 — Memory consolidation

`/memory flush` checkpoints decisions and patterns into the knowledge store with evidence links; `/memory consolidate` is a cron-lease graph template that folds session summaries into topic notes with provenance; the existing FTS + vector retrieval and file-watcher reindex are reused. **AC:** flush writes typed records with provenance; consolidation produces a new revision and never destroys its sources; the scheduled run is visible in `/jobs`. **Depends on:** `SEAM-03`.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** `crates/knowledge` is a preference/feedback model with no call site; FTS exists in the ledger and context-engine, vector index types exist unwired, and **there is no file watcher** (reindex is a per-turn walk; `/context reindex` is a stubbed refusal) — "file-watcher reindex" is struck from the item; `context_engine::memory::MemoryStore` has no production caller and is the store `flush` targets.

#### SEAM-14 — Passive update notice

Opt-in (`[update] check = true`), TTY-only, skipped in CI, non-TTY and headless runs, at most once per 24 h with a 3 s timeout, state file under `RAPIDLM_HOME`, one line at exit, using the existing manifest URL, egress under policy with a receipt. **AC:** throttling, CI skip, no hang without network, receipt recorded. **Depends on:** nothing.

**Audit correction (SEAM-00-1, 2026-09-20; [Phase 0 audit](seams-phase0-baseline-2026-09-20.md) §3):** there is no existing manifest URL — `rapid update` requires `RAPIDLM_UPDATE_URL` and exits 2 without it (audit §5 D-3, recommendation: `[update] url`, no built-in default); the manifest's `signatures` field is documented and never parsed.

#### SEAM-15 — Agent dashboard

A cross-session overview backed by the daemon: sessions, forks, background loops, pinned agents, last-turn summary, a new-agent action. A projection of daemon/ledger state, nothing stored separately. Deferred until `SEAM-03` and `SEAM-04` have landed. **AC:** every row is derivable from the ledger; killing and restarting the daemon reproduces the dashboard.

## 5. Delivery protocol — for the agents doing the work

### 5.1 Phase 0 — check the code

The first session produces, before writing any feature code:

1. **Baseline audit** `docs/goals/seams-phase0-baseline-<date>.md`, in the shape of `gvs5h-phase0-baseline-2026-09-17.md`: commit, dirty working state (preserved untouched), toolchain, required checks, supported-platform contract; then **for every `SEAM-*` item** the ground truth with `file:line` — what exists, what is reachable from `apps/rapid`, what is missing, every "today" claim in §4 confirmed or corrected, and the resulting real scope. Effort labels come from this audit, not from §4.
2. **ADR(s)** for the decisions that change contracts: at minimum the hook result v2 contract, inbox edges and delivery modes, and plan proposals. Numbered from the next free ADR number.
3. **Worklist** `docs/goals/seams-implementation-tasks.json`, same schema as `gvs5h-implementation-tasks.json` (`id`, `title`, `phase`, `status`, `depends_on`, `acceptance_criteria`, `owners`, `implementation`, `validation`, `evidence`), one or more tasks per item, dependencies explicit, Tier 1 first. Task IDs are `SEAM-<item>-<n>`.
4. **`docs/development-ledger.md`** gains the program's entries.

Phase 0 ends with a commit whose message names the audit; the first Tier 1 task starts in the same session.

### 5.2 Slice rules — plan, then execute

- Work one ready task at a time in dependency order; within a tier, in catalog order unless the audit reordered.
- Before editing, restate the task contract: affected modules and schemas, migration impact, the named acceptance evidence. Spend the first minutes grepping for the primitive (S7).
- Implement through existing seams. Never add a second scheduler, registry, store, policy engine or context authority. A new schema is versioned and gets a fixture under `crates/protocol`.
- Cover happy, negative, boundary, cancellation and recovery cases that can occur at system boundaries. A behaviour that cannot be exercised on a platform ships a typed-unavailability test there.
- A slice ends only when every required check (§5.6) is green locally, serially.
- Each slice is recorded in the current `docs/goal-delivery-<date>.md` as a criterion table (criterion / status / evidence with test names), and the worklist entry's `status` and `evidence` are updated in the same commit.

### 5.3 Verification protocol

1. **Revert-cycle every fix and every acceptance test**: with the change reverted the new test fails for the stated reason; with the change applied it passes. Record the test name in the delivery record.
2. **Never weaken an assertion** to make a suite pass. A flaky CI job is re-run before a regression is hunted; a known flake is recorded, not accommodated in the test.
3. **Never run two `cargo` suites concurrently**; concurrent runs fabricate failures that do not reproduce serially.
4. **Self-review every feature/fix commit**: launch a background adversarial review of the commit before starting the next task; treat its confirmed findings as the next task.
5. **Windows is a gate.** Retrieve full CI logs through the jobs-logs API when a run fails; the summary view truncates.
6. Claims of completion are backed by observed output, not by the absence of errors.

### 5.4 Commit, push, report

- One logical change per commit; conventional prefix (`feat(hooks):`, `fix(setup):`, `docs(seams):`); the body names the `SEAM-*` task and the acceptance criteria it satisfies; the message never names a peer product (rule 2.1).
- Commit to `main` and push to `origin/main` after every landing; the remote reflects real progress at all times.
- In a long autonomous session, write a user-facing status at least hourly: done / in progress / blocked / decisions needed. Silence reads as "stuck".
- A task that needs a product decision (a default, a name, a scope question) records "decision needed" in the delivery record with the options and a recommendation, then moves to the next ready task. It never widens policy, changes a default or names a product to get unblocked.

### 5.5 Agent profiles and the review checklist

| Phase / activity | Profile (`agents/*.md`) |
|---|---|
| Phase 0 audit and worklist | `planner`, `context-scout` |
| Implementation | `coder` |
| Tests and revert cycles | `tester`, `verifier` |
| Every commit | `reviewer` (background adversarial review) |
| `SEAM-01`, `SEAM-06`, `SEAM-09`, `SEAM-11` | additionally `security-reviewer` |

The reviewer's checklist for this program, in addition to correctness:

- [ ] No peer product, vendor agent product or competitor name anywhere in the diff or the commit message (§2.1); provider names only where §2.2 allows.
- [ ] No side channel (S1): the new behaviour is a node, an event or a gate.
- [ ] No TTY-only prompt (S2); no silent rewrite (S3); external content fenced (S4).
- [ ] Policy only narrowed (S5); derived views derive (S6).
- [ ] Revert-cycle evidence named (S8); truthful wording in every user-visible surface (S9).
- [ ] Any egress goes through the broker with a receipt and is opt-in (S10).
- [ ] Untouched files byte-identical; defaults unchanged; JSONL and exit codes unchanged (S11).

### 5.6 Required checks

Run serially, all green, before any commit:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo test --locked -p protocol --test schema_fixtures
pnpm generate:check
pnpm typecheck
pnpm test
```

Platform contract: macOS (Apple silicon) and Linux x86_64 are build, lint and test gates; Windows x86_64 is a build, lint **and test** gate. Unsupported paths on Windows report typed unavailability and are tested as such.

## 6. Definition of done — program

- Every `SEAM-*` item is either delivered with all its acceptance criteria evidenced in a delivery record, or explicitly re-scoped by an ADR that names what was dropped and why.
- The worklist has no task in a non-terminal status.
- `docs/getting-started.md`, `docs/reference/`, the hooks and permissions pages, and `docs/V3-CHANGELOG.md` describe the delivered behaviour; `docs/requirements-traceability.md` maps each item to its tests.
- `rapid doctor` on a fresh machine leads a user to `rapid setup`; `rapid setup` leads to a verified configuration; the getting-started walkthrough is re-run against the shipped binary and its transcript is attached to the final delivery record.
- No peer product is named in anything the program produced (rule 2.1); the files it touched no longer name one (rule 2.4).

## 7. Out of scope — do not build

- A projected or network-mounted working tree; any filesystem driver.
- Feedback, upsell or account modals; anything that assumes a vendor backend.
- Media generation tools; chart or diagram rendering in the TUI; bidirectional-text or high-refresh rendering work.
- A desktop-application sidecar or a second daemon; RapidLM already has `rapid daemon`.
- A second scheduler, job registry, policy engine, memory database or context authority — under any name.
- Restrictions enforced only by prompt text; approvals that exist only on a TTY.
- A repository-wide purge of historical documents (rule 2.4).

## 8. Glossary — the program's own names

| Term | Meaning |
|---|---|
| **Seam** | A host-owned extension point: a hook event, an inbox edge, a preset, an installer, a status payload. |
| **Hook result v2** | The versioned JSON a hook may print: decision, reason, `updated_input`, `additional_context`. |
| **Ask** | A hook or elicitation decision that becomes a Human/Approval wait on the graph. |
| **Rewrite marker** | The transcript line and journal record that make a hook's input rewrite visible. |
| **Background node** | A Process/Agent node the turn no longer waits on; completion wakes the session. |
| **Loop** | A cron-lease-scheduled background Agent node with a lifetime and a cap. |
| **Monitor node** | An event-driven node turning a command's output lines into bounded notifications. |
| **Resume summary** | The host-generated block listing non-terminal nodes at resume time. |
| **Inbox edge** | A typed message edge into an Agent node with a delivery mode: interject, steer, queue. |
| **Role overlay** | A file-defined narrowing of a registry role: instructions, model, effort, declared inputs/outputs. |
| **Plan proposal** | A schema-validated, read-only-produced plan that approval compiles into graph nodes. |
| **Preset** | A data row describing an endpoint family for `rapid setup`. |
| **Probe** | The minimal live request that verifies a configuration before it is persisted. |
| **Continuation** | A follow-on request stitched onto a length-truncated response, journaled with provenance. |
| **Installer by shape** | Locating and editing an external host's configuration by its structure, never its product name. |
| **Status payload** | The schema-versioned JSON a status-line command receives on stdin. |
| **Reclaimable** | A worktree whose transaction is published or abandoned, unreferenced by any lease, with nothing unpublished. |
| **Egress receipt** | The broker's record that a network call was policy-classified and allowed. |
