# RapidLM CLI vs Claude Code CLI & Grok Build — Three-Way Parity Analysis

- **Date:** 2026-08-28 (revision 2 — adds Grok Build as a second, fully source-grounded reference)
- **RapidLM side:** this workspace @ commit `abe91b0` (main), Rust workspace, 27 crates. Re-verified at HEAD:
  the exec loop still wires exactly two tools (`workspace.write` ≤8 KiB, `workspace.read` ≤4 KiB,
  `apps/rapid/src/exec_tools.rs`), and the 12-tool `GatewayTool` catalog remains defined but unwired.
- **Grok Build side:** `xai-org/grok-build` @ `9684fa3` ("Synced from monorepo"), full open-source Rust tree
  at `/Users/mohsin/grokbuild` — 78 crates under `crates/codegen` plus shared crates. **Every Grok Build
  claim below is grounded in source** and cites paths in that tree. This is the first revision of this
  document with a fully inspectable reference implementation.
- **Claude Code side:** v2.1.220/2.1.241, reconstructed from the local reverse-engineering case at
  `/Users/mohsin/claude/work/claude-code-cli` — 13 extracted system prompts, `sdk-tools.d.ts`
  (3,800-line typed SDK tool surface), npm wrapper, Mach-O binary characterization. Claude Code's
  implementation source is not available; claims are grounded in its embedded prompts and schemas.
- **Severity scale:** `P0` = blocks core agent usefulness · `P1` = major competitive gap ·
  `P2` = polish/differentiator gap · `✓` = RapidLM at parity or ahead.

---

## Executive summary

Adding Grok Build changes the character of this analysis. Grok Build is (a) Rust, like RapidLM, so its
contracts are directly transplantable; (b) architecturally a cousin (TUI frontend + agent runtime + tool
registry + MCP/hooks/plugins); and (c) production-proven against the same model-callable-surface problems
RapidLM has not yet wired. For most gaps below there is now a **concrete, copyable contract with a source
path**, not just a behavioral description of a closed binary.

The headline numbers, three ways:

| | Claude Code | Grok Build | RapidLM |
|---|---|---|---|
| Model-callable tools | **49** (43 SDK schemas + 6 runtime-only) | **≈45** (29 native + 12 ported from codex/opencode + 4 meta/memory) | **2** wired; 12 cataloged, unwired |
| Tool execution | Parallel (prompt policy) | **Parallel** (`FuturesUnordered`, per-path write locks) | Sequential, cap 16/step |
| Tool results to model | First-class per-call `tool_result` blocks (text content) | First-class per-call `tool_result` items (text + optional images) | Flat text report folded into a user message |
| Permission modes | 6: default \| plan \| acceptEdits \| auto \| dontAsk \| bypassPermissions | Same 6 | Binary project trust |
| Kernel sandbox | Exists (mechanism unnamed; `dangerouslyDisableSandbox` flag) | Landlock/Seatbelt via `nono` + child seccomp, profiles | `sandbox` crate exists, unwired |
| Hooks | 2 evidenced (PreCompact, user-prompt-submit) | **15 events, blocking semantics** | Plugin hook dry-run only |

The five gaps that matter most, in order:

1. **Tool breadth in the loop (P0)** — unchanged from revision 1; now with two reference tool inventories
   to converge on.
2. **Per-call tool-result channel (P0, narrowed)** — *correction to revision 1:* neither reference sends
   typed structs to the model. Both send one result block per call with **text** content (Grok Build:
   `ToolResultItem { tool_call_id, content, images }` in `xai-grok-sampling-types/src/conversation.rs`;
   Claude: `tool_result` content). RapidLM's single flat "tool results:" report inside a user message is
   non-standard on two counts (not per-call, not result-role). The fix is smaller than previously scoped.
3. **Parallel tool calls (P1)** — Grok Build's implementation is the copyable design: dispatch all calls in
   one assistant message concurrently; serialize only writes to the same path
   (`xai-grok-shell/src/session/acp_session_impl/tool_calls.rs` + `tool_dispatch.rs`).
4. **Permission lattice (P1)** — both references independently converged on the **same six modes** plus
   `Tool(glob)` allow/ask/deny rules, remembered grants, and read-only auto-allowlists. Adopting that exact
   lattice buys Claude-compat for free (Grok Build reads `.claude/settings.json` for it).
5. **Prompt/context stack (P1)** — Grok Build's template-renderer + `PromptContext` is a simpler mechanism
   than Claude's ~25 dynamic sections and is the right weight for RapidLM to adopt first.

Where RapidLM genuinely leads (verified against both references): **durable, evidence-gated agent work**
(ledger-backed sessions, goal completion gated on cited deterministic checks, cron with claim-lease firing
and corruption quarantine — both references' schedulers default to non-durable/7-day-expiry), **routing
policy** (llm-router filter/score/fallback chain; Grok Build pins one provider with a circuit breaker),
**computer-use breadth** (browser/desktop/mobile crates — neither reference ships the equivalent in-CLI),
and **test discipline** (~2,900 tests, `#![forbid(unsafe_code)]`).

A notable convergence: Grok Build also ships a goal system (`/goal`, `update_goal` with
completed/blocked signals, "evidence-verified completion") — all three tools now agree that goal-gated,
verification-anchored work is the right primitive. RapidLM's implementation is the deepest but is
CLI-only; the model cannot touch it.

**Corrections to revision 1 (Claude-side):** ① tool count is 43 SDK + 6 runtime-only, not "~40"; the missed
schemas are `ListMcpResources`, `ReadMcpResource`, `ReadMcpResourceDir`, generic `Mcp`,
`ShowOnboardingRolePicker` (`RefreshMcpTools` was already counted). ② Claude's `Workflow` is a JS-dialect
script API (`meta` + `agent()/parallel()/pipeline()/phase()`), **not Rhai** — Rhai is Grok Build's workflow
engine (`xai-workflow` crate). ③ permission modes include `auto` (6 total). ④ PowerShell has no schema —
it is the Windows runtime variant of Bash. ⑤ only `PreCompact` and `user-prompt-submit` hooks are evidenced
for Claude; do not assume a fuller list. ⑥ six bundled skills (pdf, docx, pptx, xlsx, **pdf-reading,
frontend-design**). ⑦ the orchestrator "ReportFindings review pass" is not evidenced; only the tool schema
(`verdict: CONFIRMED|PLAUSIBLE`) is.

**Corrections to revision 2 (RapidLM-side, 2026-08-29 audit — the headline table above and "five gaps"
list below are now substantially wrong):** the commit immediately after this doc's `abe91b0` snapshot was
titled `ac66e8a Wire the gaps.md parity core into the exec loop`, followed by ~15 more commits that
implemented most of what this section calls the top five gaps — the doc was never refreshed. Five
independent code-reading passes on 2026-08-29 (full detail in §18/§19) found: **tool execution is parallel**
(`std::thread::scope`, per-path write-group locks, 16/step cap — gap 3 is done), **tool results are
per-call** (one `Tool`-role `CanonicalMessage` per call with its own id — gap 2 is done), **permission
modes are the same six** with `Tool(glob)` rules and remembered grants, not "binary project trust" — gap 4
is done, **hooks are real and blocking** for Pre/PostToolUse (not "dry-run only" — though Prompt/Stop
stages and a typed event enum are still missing), and **tool breadth (gap 1) is much wider** than "2 wired,
12 cataloged" — read/glob/grep/patch/shell/todo/plan/ask_user/task_spawn/web_fetch/mcp are all live
(`apps/rapid/src/exec_tools.rs`), leaving gap 5 (prompt/context stack) as the only one of the five still
accurately open (`PromptContext` now exists but stays 3 sections, not Grok's fuller template renderer).
The "where RapidLM leads" claims fare worse under the same audit: **post-compaction verification** and
**multi-provider routing/fallback** are real, tested code with **zero call sites** from the live turn
loop — orphaned, not shipped; **computer-use breadth** remains genuinely unwired. Only the durability
claims (ledger-backed sessions, cron quarantine) held up unchanged. Treat every `✗`/gap number above this
note as unverified until cross-checked against §18/§19's 2026-08-29 status columns.

---

## 1. Product shape & distribution

| | Claude Code | Grok Build | RapidLM |
|---|---|---|---|
| Runtime | Single Bun 1.4.0 Mach-O (257–325 MB, arm64), hardened-runtime signed | Native Rust binary (`xai-grok-pager-bin` → `grok`), ~78 crates | Native Rust workspace binary |
| Distribution | npm `@anthropic-ai/claude-code`, 8 platform optional deps | curl installer (macOS/Linux/Windows), self-updater (`xai-grok-update`), DotSlash for build tools | Build from source; signed-updater stub only |
| Surfaces | CLI, desktop app, web (claude.ai/code), IDE extensions, Agent SDK | TUI, headless, ACP server (stdio + WebSocket), official ACP SDKs (TS/Rust/Python/Go/Kotlin), Grok Desktop | TUI, headless exec, ACP crate (v1/v2, stdio, compat tests), daemon/IPC in kernel crate |
| SDK/contract artifact | `sdk-tools.d.ts` typed tool schemas | `xai-grok-tools-api` protobuf tool API; headless JSON schema | None exported |

**Gap (P2).** Unchanged in substance from revision 1: export a typed tool-schema artifact from
`tool-gateway::schema` and pick a headless JSON contract (§15). Grok Build's `xai-grok-tools-api`
(protobuf tool API definitions) is the closest open reference for a versioned tool contract.

---

## 2. Model-facing tool surface — the core gap (P0)

**Claude Code (49):** Bash (bg, timeout ≤600 000 ms, auto-background on timeout, Ctrl+B, sandbox override),
PowerShell (Windows Bash twin, no schema), Read (offset/limit, PDF ≤20 pages/call, token-cap auto-pagination
`truncatedByTokenCap`, `file_unchanged` dedup), Write, Edit, Glob (truncated at 100 files), Grep
(head_limit default 250, offset), NotebookEdit, WebFetch, WebSearch (domain allow/block), TodoWrite,
AskUserQuestion (1–4 q × 2–4 opts, previews, header ≤12 chars, `afkTimeoutMs`), Agent (Explore/Plan/
general-purpose/fork; background default; worktree/remote isolation; model aliases sonnet/opus/haiku/fable;
naming + SendMessage), TaskCreate/Get/Update/List, TaskOutput/TaskStop, EnterPlanMode/ExitPlanMode,
REPL (stateful JS, 30 s default/600 s max), Workflow (JS dialect, `scriptPath`, `resumeFromRunId`),
CronCreate/Delete/List (5-field cron, 7-day expiry, `durable` → `.claude/scheduled_tasks.json`),
ScheduleWakeup (60–3600 s), Monitor (command xor WebSocket, persistent), EnterWorktree/ExitWorktree
(name ≤64 chars, exit `discard_changes` required when dirty), Artifact (publish/list, 409 + `force`),
Projects (RAG knowledge, 5 methods), ReportFindings (≤32, CONFIRMED/PLAUSIBLE), SendFeedback,
PushNotification (<200 chars), RemoteTrigger, ProposeSkills (1–3), ClaudeDesign, Skill, MCP meta
(`Mcp` executor + resource tools + `RefreshMcpTools` that "never dials"), ShowOnboardingRolePicker,
advisor, SendUserMessage, SendMessage/ListAgents, subscribe_pr_activity.

**Grok Build (≈45):** native `GrokBuild` namespace — `run_terminal_cmd` (timeout default 120 s, foreground
ceiling up to 36 000 s configured, output cap 20 000 chars, auto-background on timeout, streaming bash
deltas ≤16 KiB/frame), `read_file` (offset/limit default 1 000 lines/25 000 tokens, PDF ≤20 pages/call,
text/PDF/PPTX/ipynb/image), `search_replace` (exact old/new, must differ, `replace_all`, unicode-fallback),
`list_dir` (10 000-char budget), `grep` (head_limit 200/500, hard caps 2 000/10 000, 40 KB output),
`task` (subagent spawn; bg default; worktree isolation; `resume_from`; depth ≤1), `send_subagent_message`,
`get_task_output`/`wait_tasks` (≤20 ids, 600 s wait ceiling)/`kill_task`, terminal-output/kill variants,
`todo_write` (merge-by-id semantics), `update_goal` (completed/blocked), `workflow` (Rhai, agent_budget
1–1024), `web_search` (domain caps, backend search on grok-4.6), `web_fetch` (URL ≤2 000, 10 MB body,
100 KB inline cap, SSRF guard, domain allowlist, proxy), `lsp` (goToDefinition/findReferences/hover/
implementation/documentSymbol/workspaceSymbol), `image_gen`/`image_edit`/`image_to_video`/
`reference_to_video` (batch caps 8/4), `enter_plan_mode`/`exit_plan_mode`, `ask_user_question`
(1 800 s default wait), `monitor` (10 h cap, persistent), `scheduler_create`/`list`/`delete` (human
intervals, 50 max, 7-day expiry), plus ported sets: codex (`apply_patch`, `read_file`, `list_dir`,
`grep_files`) and opencode (`bash`, `read`, `edit`, `write`, `grep`, `glob`, `todowrite`, `skill`),
MCP meta (`search_tool` BM25 discovery with 2 048-char descriptions, `use_tool` qualified dispatch),
memory (`memory_search`, `memory_get`). Registration: `xai-grok-tools/src/registry/types.rs`
(`ToolRegistryBuilder::new()`). Tool IDs are namespaced (`GrokBuild:read_file`, `Codex:…`, `OpenCode:…`)
and every tool carries `is_read_only` + `ToolScope{Read,Write}` used by permissions.

**RapidLM (2):** `workspace.write`, `workspace.read` — behind project trust only.

**The architectural irony (unchanged):** `tool-gateway` already defines a 12-tool v1 catalog
(`RepoSearch, RepoRead, WorkspacePatch, WorkspaceStatus, ShellExec, AgentSpawn, AgentResult, GoalUpdate,
BrowserAct, MobileAct, ExternalCall, EvidenceRecord`) with per-tool JSON Schema, capability scoping, deny
lists, and repair logic. The capability broker, sandbox, and process supervisor exist. None are reachable
by the model.

**Recommendation (unchanged, now with a worked example):** build the `GatewayTool` → `ToolDriver` adapter
and roll tools out individually. Grok Build is the existence proof that this is the whole game: its
permission rules, plan-mode gate, sandbox auto-approval, and subagent capability modes all hang off the
same per-tool registry metadata RapidLM's `ToolDriver::tool_surface()` → `CanonicalToolSpec` plumbing
already carries. Priority order: `RepoRead` (paginated), `RepoSearch` (rg flags + head_limit/offset),
`WorkspacePatch` (exact-match edit), `ShellExec` (sandboxed), `AgentSpawn/AgentResult`, then todo,
question, and goal tools.

---

## 3. Tool-call mechanics

| Mechanic | Claude Code | Grok Build | RapidLM | Gap |
|---|---|---|---|---|
| Result channel | Per-call `tool_result` blocks, text content; typed outputs rendered client-side | Per-call `ToolResultItem{id, content, images}`; typed `ToolOutput` used for ACP/telemetry only | Flat "tool results:" text folded into one user message | **P0 (narrowed — see summary correction)** |
| Parallel calls | Prompt-mandated parallelizing | **Default parallel** (`FuturesUnordered`); per-path `tokio::Mutex` serializes only same-path writes; reads fully concurrent | Sequential, cap 16/step | **P1** |
| Turn call cap | None found | No hard cap (media batch caps 8 image/4 video) | 16/step const | ✓ (bounds-as-consts is fine; lift only if needed) |
| Read pagination | offset/limit, token-cap auto-pagination, `file_unchanged` dedup | offset/limit (1 000 lines/25 000 tokens), PDF pages, negative offsets | 4 KiB prefix + `[truncated]` | **P1** |
| Edit semantics | Exact old/new, `replace_all` | Exact old/new, must-differ, `replace_all`, unicode-normalized fallback | Whole-file overwrite only | **P1** |
| Rich reads | Images, PDFs, notebooks | Images (inline in results), PDFs, PPTX, ipynb | UTF-8 text only | P2 |
| Retry | Not characterized in evidence | Typed `is_retryable()` taxonomy (5xx/conn/stream retryable; 4xx non-429, auth not), 15-attempt cap, 30 s jittered backoff, `Retry-After`, `x-should-retry`, one shared auth-recovery retry per batch, tool-level backoff 10/1 s/30 s | Cause classes (auth/connection/provider/transient), bounded retry-after-aware backoff | ✓ **lead downgraded: Grok Build matches this**; keep and extend to tool/subagent failures |
| Streaming | Yes (implied) | Full: `stream_chat_completions`, bash output chunk deltas, partial-capture persistence (`streaming_partial.json`) | Unknown/unwired in exec loop | P2 |

**Recommendation:** (a) emit one result-role message per call (OpenAI `role:"tool"` / Anthropic
`tool_result`) with text content — both references prove this is sufficient; typed structs are a
client-side concern. Land it in `apps/rapid/src/model.rs`/`llm-router` canonical messages. (b) Port Grok
Build's dispatch shape for parallelism: prepare all calls (permission → parse → meta), then
`FuturesUnordered`, taking a per-path lock only for write-classified calls — RapidLM already has the
read/write classification seam in `ToolKind`.

---

## 4. Permissions, trust & safety

Both references converged on the **same lattice**; RapidLM is the outlier.

**Modes (identical set in both):** `default` (ask; read-only auto-runs) · `plan` · `acceptEdits` ·
`auto` (classifier pre-check) · `dontAsk` (silently deny non-pre-approved) · `bypassPermissions`
(always-approve/YOLO; deny rules and hooks still apply; admin-lockable — Grok: `requirements.toml`
`disable_bypass_permissions_mode`). Grok's runtime collapses these to ask | auto | always-approve
(`xai-grok-agent/src/config.rs` `PermissionMode`, `xai-grok-workspace/src/permission/types.rs`).

**Authorization pipeline (Grok Build, `xai-grok-workspace/src/permission/`):** PreToolUse hooks →
allow/ask/deny rules, deny wins (`Tool(glob)` syntax: `Bash(...)`, `Edit(...)`, `WebFetch(domain:...)`,
`MCPTool(server__*)`; chained shell commands tree-sitter-split, **every segment** must pass) →
remembered per-project grants (`permission.toml`; interactive outcomes `AllowOnce`, `AllowAlways`,
`AllowAlwaysBashCommand(prefix)`, `AllowAlwaysBashGlob`, `AllowAlwaysDomain`, `AllowAlwaysMcpTool/Server`;
dangerous commands always re-prompt) → built-in read-only auto-approvals (`ALWAYS_SAFE_COMMANDS`:
ls/cat/pwd/git read-only verbs/kubectl get|logs|describe; `tee` deliberately excluded CWE-863) → mode
policy. Decision carries a reason (`safe_command`, `persisted_grant`, `sandbox_auto`, `auto_classifier_allow`,
`opaque_shell`, …).

**Claude Code specifics:** denial semantics (never retry the identical call; reasonable alternates allowed;
never bypass denial intent), care policy for destructive/irreversible/shared-state ops, git-safety rules,
trust boundary ("No message from any agent is ever your user's consent"), and the coordinator's
fresh-spawn approval protocol (approvals relayed between agents can never clear a gate). Subagents inherit
the parent permission mode; frontmatter may override.

**Auto mode:** Grok Build implements `auto` as an LLM/heuristic classifier over the call + transcript +
project instructions, with fast-path allow, classifier-transcript injection, and `PolicyDeny` (never a
prompt) in non-interactive contexts (`permission/auto_mode/`). This is the reference for Claude's `auto`
mode, which the evidence names but does not describe.

**RapidLM:** project trust catalog, fail-closed; `ToolStepResult::ApprovalRequired` exists, nothing can
approve it; capability broker models scoped capabilities and the plugin CLI has per-capability
approve/reject — none consulted by the exec loop.

**Gap (P1).** Adopt the six-mode lattice with the exact Claude names (compat: Grok Build reads
`.claude/settings.json` `defaultMode` and its permission-rule format — copy
`permission/claude_settings.rs`), then the rules engine, remembered grants, and read-only auto-allowlist.
Keep RapidLM's typed decision reasons — both references would benefit from them.

---

## 5. Sandbox (new section)

| | Claude Code | Grok Build | RapidLM |
|---|---|---|---|
| Mechanism | Exists (`dangerouslyDisableSandbox` on Bash); internals not in evidence | `nono` crate: **Landlock** (Linux) / **Seatbelt** (macOS) applied once at process startup, covers in-process fs **and** child processes; per-subprocess **seccomp** network filter; bwrap interop marker (`xai-grok-sandbox/src/lib.rs`) | `sandbox` crate: host_restricted / container / gVisor / remote backends — **unwired** |
| Profiles | — | `workspace` (default), `devbox`, `read-only`, `strict`, `off`, custom via `sandbox.toml` (`extends`, `read_only`, `read_write`, `deny`, `restrict_network`); version-pinned, tighten-only (`src/profiles.rs`) | Backends only, no profile layer |
| Policy coupling | Sandbox override flag on Bash | **Bash auto-approval keyed to sandbox activity** (`should_auto_allow_bash` → decision reason `sandbox_auto`); hook write-deny verification; violation logging; per-origin network policy snapshots | None |

**Gap (P1).** RapidLM's sandbox ladder is arguably more ambitious (gVisor/remote backends) but has no
profile model and no permission coupling. Copy the profile file format and the sandbox→approval coupling:
a command that ran inside the sandbox can be auto-allowed, which is what makes `default` mode usable.
Grok Build's startup-time kernel enforcement is also the simpler deployment story vs. per-exec sandboxing.

---

## 6. Context management & compaction

**Claude Code:** three LLM-summarized variants (full `/compact`, reactive recent-only, retro up-to) each
producing `<analysis>` + 9-section `<summary>`; continuation wraps transcript path + resume guidance;
**microcompaction** — keep 5 most recent tool results verbatim, stub older (`[Old tool result content
cleared]` / offload to `<persisted-output>`), skipped unless ≥20 000 tokens saved; server-side eviction
via `context-hint` beta (75 k tokens); live `<total_tokens>N tokens left</total_tokens>` injected every
turn; rejection of agent-shaped fake user turns in summaries.

**Grok Build:** auto-compact at **85 %** of context window (`DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT`,
per-model override 80 %); **full-replace** summarization (structured prompt or a short
"summarize for a successor assistant" self-summarization prompt) with degenerate-summary detection and
retry; **two-pass with prefire** — a background pass-1 sample is cached (`AsyncCompactionCache{note1,
prefix_len, fingerprint}`) so the second pass is cheap; compaction **suppression states** (turn-scoped,
sticky for size/schema failures, until-200 for credit block, auth-scoped); compacted segments offloaded to
the session dir as `compaction/INDEX.md` + `segment_*.md` with per-segment 512 KB caps and per-turn detail
levels (`xai-grok-compaction`, `xai-compaction-transcript`); per-call output caps (40 KB tools, 20 KB MCP)
do the rest; **tool-result pruning** in memory config (keep last 3 turns, soft-trim 4 000 chars, hard-clear
at 10, decay half-life 30 d); `/context` shows a per-tool/skill/MCP token breakdown; post-compaction turns
use a short `COMPACT_SYSTEM_PROMPT`.

**RapidLM:** `context-engine::compact` with soft/hard thresholds, strategy layer, **post-compaction
verification** + evidence; `ContextRecoveryDecision` overflow recovery that never replays committed tool
effects; reminder feeds; token totals on stderr.

**Assessment:** mechanism parity is now three-way table stakes; the ergonomic deltas are: no
auto-trigger percentage in RapidLM, no stale-tool-result pruning/offload, no live token line in the TUI,
no `/context` breakdown. **Lead (✓) retained:** post-compaction verification is absent from *both*
references (Grok Build's compaction ends at the summary + short prompt; Claude's at the 9-section summary).

**Recommendation (P2):** auto-compact threshold config + keep-last-N tool-result pruning (Grok Build's
keep-3/trim-4 000/hard-clear-at-10 is directly portable; Claude's keep-5/≥20 k gate is the same idea with
a savings gate) + a `/context`-style breakdown, which RapidLM's packet compiler can already produce.

### 6a. The retrieval layer is built and unwired (P0, found 2026-08-29)

The assessment above is about compaction ergonomics — packing/budgeting an *already-assembled* context
packet. It understates a much bigger gap underneath it: `context-engine` is 29,109 lines across `chunk`,
`compact`/`compact_policy`, `compile`, `index::{fts,vector,graph}`, `ingest::{walk,watch,content,pipeline}`,
`lsp`, `memory`, `parse::{registry,symbols}`, `read`/`read_set`, `repo_manifest`, `retrieval::{candidates,
rank,grep,links,filter}`, `scout`, `token_estimate` — with 305 passing tests. Design-wise this is more
sophisticated than either reference: a code graph, vector/embedding search, LSP integration, Tree-sitter
symbol parsing, an ingest/watch pipeline, and a Context Scout (`scout.rs`) that reports negative findings
and tracks freshness/lineage, not just hits.

**Traced the actual call path and confirmed almost none of it is reachable from `rapid exec`.**
`apps/rapid/src/host.rs::build_packet()` — the *only* place a live turn builds a `ContextPacket` — calls
`compile()` with nothing but fixed prompt-stack text: `ctx.system("system/prompt", …)`,
`ctx.system("memory/index", …)`, `ctx.system("rules/agents", …)`, `ctx.system("skills/selected", …)`,
`ctx.goal(…)`, per-criterion `ctx.goal_block(…)`, `ctx.memory("context/compaction", …)`,
`ctx.system("reminders/active", …)`. Zero calls into `scout`, `retrieval::candidates`, `index::{fts,
vector,graph}`, `lsp`, or `ingest`. Of the crate's 29,109 lines, only `compile.rs` + `compact.rs` +
`compact_policy.rs` (~2,400 lines, ~8%) are wired — the packing/budgeting layer. Even
`context_engine::memory` (1,640 lines) is bypassed: `host.rs::load_memory_index` reads `.rapidlm/MEMORY.md`
with a bare `fs::read_to_string` + line/byte truncation instead of the real module.

**This is not a hypothesis — it explains something measured directly in the same-day benchmark work**
(`docs/benchmarks/2026-08-29-hard-swe-context-history-bug.md`, `2026-08-29-post-fix-reverification.md`):
every run starts the model at zero repo knowledge, so it manually `repo_glob`s then `workspace_read`s every
file one at a time before touching anything, burning steps/tokens that a wired retrieval layer would have
front-loaded. It is also the reason the `MAX_TOOL_HISTORY_*` budget fix from that session mattered so much
— the model was paying full exploration cost on *every single turn*, with no memory of the repo carried
in from a prior index.

**Comparison:** Grok Build has nothing this ambitious — its closest equivalent is
`xai-fuzzy-file-search` (936 lines, fuzzy filename matching, not semantic) in the reference tree at
`/Users/mohsin/grokbuild` — but it *is* an actual dependency of `xai-grok-workspace`, i.e. wired into the
real tool loop. Qwen Code's source isn't available for comparison; the two same-day benchmark transcripts
against it (`2026-08-28-qwen-code-vs-rapid.md`, `2026-08-29-swe-repo-fix.md`) show it doing the same
manual, one-file-at-a-time exploration RapidLM does (11 `read_file` calls to explore a five-module ~150 LOC
repo) — no visible pre-populated retrieval either, though a hidden layer that simply didn't trigger on a
small repo can't be ruled out without source. The honest read: RapidLM's retrieval design is more
sophisticated than either reference's, and that design currently buys nothing, because a working
fuzzy-filename search that ships beats a code-graph-plus-vector-index stack that doesn't run.

**Recommendation (P0):** wire `build_packet()` (or a new pre-turn step ahead of it) to source real
candidate content via `context-engine`'s existing retrieval before the turn starts, using the task
prompt/goal as the query, and feed the results in as scored/optional blocks `compile()` already knows how
to budget and drop. Start with FTS (`index::fts` + `retrieval::{candidates,rank}` via `scout.rs`) — no
embedding-model dependency, no persistent-index staleness problem, and it directly targets the observed
failure mode (the model rediscovering the same files by hand every run). Vector search, the code graph, and
LSP are larger, separate investments once FTS proves the wiring; `context_engine::memory` replacing the
naive `load_memory_index` is a small, independent follow-up.

**Implemented 2026-08-29.** New module `apps/rapid/src/context_retrieval.rs`: for a trusted project,
`retrieve(root, task_prompt, budget)` builds/updates a persistent FTS+code-graph index under
`.rapidlm/index/` via `IndexPipeline::open` (incremental — unchanged files are a content-hash no-op),
walks the repo (`ingest::walk`, capped at `MAX_INDEXED_FILES`), runs `scout()` with the task prompt as the
question, and converts `Reference` hits into `CompileInput`s (`CompileReason::Retrieved`,
`TrustClass::Untrusted`, `Freshness::Fresh` — `compile()`'s own retrieved-share budget does the trimming).
Wired through a new `PreservedLiveContext::with_retrieved_context`/`retrieved_context()` field into
`build_packet()`. Bounded by a background-thread wall-clock watcher (`RETRIEVAL_TIMEOUT`, 8s) that fires
`CancellationToken::cancel()` on the cooperative-cancellation walk/index/scout loop; any error anywhere in
the pass fails open (empty result, one-line stderr warning) — it can never block a turn. Called from
`interactive.rs::exec_turn` only when the workspace is trusted (it writes the index to disk).

Live-verified against the real model: the same LRU-cache repo used for the context-history-bug benchmark,
asked "explain what LRUCache does and how eviction works" — before this change every benchmark today
needed a `repo_glob` + `workspace_read` round trip to answer a question like this; with retrieval wired in,
stderr showed `context retrieval: 6 block(s) proactively retrieved`, and the model answered correctly and
in detail (`_touch`, `_order`, capacity, recency all named correctly) in **one model step with zero tool
calls**. 6 new tests in `context_retrieval.rs` plus an integration test in `host.rs` asserting a retrieved
block reaches the compiled `ContextPacket` with the right source/trust/freshness; full workspace test suite
green.

**Not done in this pass:** vector search and LSP stay unwired (as scoped above); `workspace_patch`-style
incremental re-indexing on edit isn't hooked up (each turn re-walks from the persistent index, which is
fast after the first cold index, but a mid-turn edit to a file won't be reflected in *that same turn's*
retrieval since retrieval runs once at turn start); `context_engine::memory` replacing `load_memory_index`
is still open.

---

## 7. System prompt & behavior stack

**Claude Code:** ~25 conditional dynamic sections (24 named in emit order: anti_verbosity, pronouns,
action_caution, task_continuity, fable_identity, tool_param_json, investigate_first, session_guidance,
memory, env_info, language, output_style, bg-session, scratchpad, context_management, brief, focus_mode,
act_dont_rederive, delivering_work_max, overcorrection, subagent_steer_delegation, heron_brook,
autonomy_append, endconv_deferred_hint) + fixed sections (# System / # Doing tasks / # Executing actions
with care / # Using your tools / # Tone and style) + 3 intro variants (CLI / Agent SDK / SDK-only) + a
lean "# Harness" replacement for small models + billing/attribution header + per-turn token counter;
output styles (Proactive/Explanatory/Learning) swap the opening; 11 auxiliary fragments; a cache marker
(`__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__`) delimits the dynamic region for prompt caching.

**Grok Build:** template files (`templates/prompt.md`, `subagent_prompt.md`, `apply_patch_prompt.md`,
XOR-obfuscated at rest) rendered by `TemplateRenderer` with `${tools.by_kind.*}` substitution and
`${%- if %}` conditionals evaluated against the **live tool registry**; `PromptContext` carries
prompt_mode, audience (Primary/Subagent — subagents get a compact template), `TemplateOverride`
(None/Codex/custom), agents_md files, persona/role instructions, memory state, env info, and
`is_non_interactive`; post-compaction sessions swap to `COMPACT_SYSTEM_PROMPT`;
`--append-system-prompt` / `--system-prompt-override` flags. The doc `<user_guide>` section is injected
only when TUI docs exist. (`xai-grok-agent/src/prompt/`, `templates/`.)

**Both:** AGENTS.md discovery is cwd → repo root, nested and accumulating, plus home-dir and vendor-compat
locations. Grok Build loads **any of** `Agents.md, Claude.md, CLAUDE.md, CLAUDE.local.md, AGENT.md,
AGENTS.md` per directory plus `.grok/rules/*.md`, `.claude/rules/`, `.cursor/rules/`, with **no size
cap**, re-discovery when files change mid-session (`AgentsMdTracker`), and injection partitioned by
workspace-vs-user scope. Claude's MEMORY.md is always loaded at 200 lines / 25 000 bytes.

**RapidLM:** role + permissions profile in the agent spec; bounded reminder feeds; goal/orchestration
prompts. No dynamic-section compiler, no model-tier variants, no output styles, no AGENTS.md convention.

**Gap (P1).** Adopt Grok Build's mechanism (it is the lighter of the two and the source is available):
a template + context struct with conditional sections (env, trust state, token budget, cause-class
remedies, sandbox posture), a subagent audience variant, and a post-compact short prompt — all emitted
with the same bounds discipline as `reminders.rs`. Then adopt the AGENTS.md discovery convention with the
`.claude`/`.cursor` compat paths (cheap, and both references do it).

---

## 8. Subagents, orchestration & background work

**Convergence point:** *both references ship the same three built-in subagent types* — Claude:
Explore (read-only; bash allowlist `ls, git status/log/diff, find, grep, cat, head, tail`; breadth
parameter), Plan (read-only, must end with "Critical Files for Implementation"), general-purpose
(`tools: ["*"]`), plus fork. Grok Build: `general-purpose` (default), `explore`, `plan` (read-only),
extensible/shadowable from `.grok/agents/*.md` + `~/.grok/agents/` with frontmatter
(`tools`, `mcpInheritance: all|none|named|except`, `permissionMode`), capability modes
read-only/read-write/execute/all, personas/roles with model + effort overrides, **depth limit 1**,
background-by-default with `resume_from` transcript continuation, worktree isolation
(`xai-grok-subagent-resolution`, task tool `task/mod.rs`).

**Background work:** Claude — bg Bash (timeout auto-backgrounds, Ctrl+B), cron (5-field, 7-day expiry,
durable JSON), ScheduleWakeup (60–3600 s), Monitor (stdout xor WebSocket, persistent), Workflow (JS,
`scriptPath`, `resumeFromRunId`, remote dispatch to CCR), bg sessions (`CLAUDE_CODE_SESSION_KIND` with
**enforced worktree isolation for edits**, auto-ship = commit→push→draft PR). Grok Build — bg terminal
commands, monitors (10 h cap or persistent), `/loop` + `scheduler_*` tools (human intervals, 50 max,
7-day expiry, non-durable by default), background subagents, **workflows (Rhai scripts with `meta`,
`args`, persisted `script_path`, `resume_from_run_id`, `agent_budget`, host-capped live children)**,
`/goal`, `/deep-research` (verifier shards), wait/kill task tools, completion notifications injected into
the conversation, still-running status line, tasks pane.

**RapidLM:** `agent-pool`, scheduler (graph runs, playbooks, **ledger-backed cron with claim-lease firing
and corruption quarantine**), goal driver, worktree hygiene, control-room projection, `rapid process`,
external agent defs (`.rapidlm/agents/*.toml`).

**Assessment:** durable orchestration: **✓ ahead** (both references' schedulers are in-memory or
file-JSON with 7-day expiry; RapidLM's cron survives restarts with lease semantics). Model-callable
orchestration: absent — the model cannot spawn a subagent, watch a log, wait on a task, or run a
workflow (**P1**, unchanged). The Explore/Plan split (cheap read-only fan-out with prompt + tool-subset
enforcement) is the highest-value missing pattern, and *the exact type names are now standardized across
both references* — adopt `general-purpose`/`explore`/`plan` verbatim. Grok Build's `wait_tasks`
(≤20 ids, bounded block) and `resume_from` (continued worker keeps full transcript) are the two contract
details worth copying exactly. Note RapidLM's V3 invariant "subagents receive minimal typed envelopes,
not the full transcript" deliberately diverges from both references here — keep the divergence, but make
it a flag, since `resume_from`-style continuation is genuinely useful for long verification children.

---

## 9. Memory & knowledge

| | Claude Code | Grok Build | RapidLM |
|---|---|---|---|
| Index file | `MEMORY.md` always loaded, 200 lines / 25 000 bytes, one-line entries | `~/.grok/memory/MEMORY.md` (global) + per-workspace (keyed on origin remote); first-turn injection (min_score 0.9) | None |
| Records | 4 typed frontmatter files (user/feedback/project/reference), `[[name]]` links, exclusions list, re-verification-before-use rule, team scope `private:`/`team:` via memory-service, secrets ban | SQLite FTS5 (+ vec0 vectors when embedding model set); `/flush` LLM-summarized session write (≤8 000 chars), `/dream` consolidation (24 h/5-session gates), MMR λ0.7, search min_score 0.7, `memory_search`/`memory_get` tools | `context-engine::memory`, repo manifests, retrieval/index/LSP — no productized surface |
| Guardrails | Exclusions apply even when user asks; stale memory must be re-verified | Session-log decay; chunk caps 1 600 chars | — |

**Gap (P2).** Unchanged in substance: a bounded always-loaded pointer file + typed records is
trivially reachable from `context-engine::memory`. Grok Build adds two ideas worth copying: keying the
workspace memory on the git origin (clones/worktrees share memory) and the `/flush`-before-compaction
hook (memory write integrated with the compaction path).

---

## 10. Failure handling, diagnostics & retries

**Claude Code:** behavioral policies only (denied-permission workarounds, care policy); no
CLI-contract-level failure taxonomy in evidence.

**Grok Build:** typed error taxonomy with `is_retryable()` (5xx except 525/526, connection, mid-stream,
empty-response retryable; 4xx non-429, auth, idle-timeout not), 15-attempt ceiling, 30 s jittered backoff,
`Retry-After`-aware, `x-should-retry` header honored, single-flight auth recovery per batch, generic tool
retry backoff (10/1 s/30 s), crash handler crate, circuit-breaker crate, auto-compaction suppression
states distinguishing context-length from transient failures.

**RapidLM:** cause classes (authentication/connection/provider rejection/transient/unspecified) with
remedies, one-line CLI messages, nonzero exits, bounded retry-after-aware cancellable backoff, no-replay
of committed effects, `--verbose` per-attempt diagnostics, host-labeled bounded logs.

**✓ → at parity with Grok Build, ahead of Claude's evidence.** Revision 1 claimed this as a RapidLM lead;
Grok Build matches it in kind (the differentiator left is RapidLM's **no-replay recovery guarantee** and
ledger-quarantined cron, which neither reference states). Extend the taxonomy to decorate future tool and
subagent failures (`ProviderFailed{Auth}` should surface as such), and add Grok Build's suppression-state
idea for compaction-on-failure.

---

## 11. Verification & evidence culture

**Claude Code:** orchestrator verification phase ("prove the code works, not confirm it exists");
`ReportFindings` with CONFIRMED/PLAUSIBLE verdicts and post-fix outcomes (schema-evidenced; the
review-pass loop itself is not).

**Grok Build:** `update_goal` tool (completed/blocked after repeated failures); `/goal` with
**evidence-verified completion** and token budget; `/deep-research` runs verifier shards; plan-mode exit
gate forces explicit approval; headless `usage_is_incomplete`/`cost_is_partial` fail-closed flags;
`--json-schema` structured output validation.

**RapidLM:** durable goal lifecycle gated on an evidence store; `goal claim` runs deterministic checks,
cites every run in the ledger, and accepts only when the host supervisor verifies every criterion;
evidence records must cite real ledger events (anti-fabrication).

**✓ Lead retained, now convergent.** All three tools converge on verification-gated completion;
RapidLM's ledger-cited evidence remains the most rigorous. `GatewayTool::EvidenceRecord` exists unwired —
wire it so the model can cite its own verification (neither reference can do that durably). Grok Build's
`update_goal` is the model-facing contract to copy for in-turn goal progress signaling.

---

## 12. Extensibility: MCP, plugins, skills, hooks

**MCP.** Claude: in-loop (call, resources, `RefreshMcpTools` that "never dials", keeps previous set on
error); transports/auth not evidenced. Grok Build: stdio + HTTP/SSE + **streamable HTTP with session-id
templating**; **automatic OAuth browser flow** (tokens 0600 at `~/.grok/mcp_credentials.json`); elicitation
support; tool output cap 20 KB; namespaced `server__tool` **plus two meta-tools** (`search_tool` discovery,
`use_tool` dispatch — keeps the toolset stable across turns to avoid KV-cache breaks); per-server
startup/tool timeouts; compat loading from `.claude.json`, `.cursor/mcp.json`, `.mcp.json`;
`grok mcp list/add/remove/doctor` (`xai-grok-mcp`). RapidLM: `mcp` crate (gateway/server/transport/trust/
catalog) + `rapid mcp-tools` + plugin trust; **nothing model-callable**.

**Skills.** Claude: `.claude/skills/<name>/SKILL.md`, 6 bundled, slash-invoked, ProposeSkills drafts.
Grok Build: `SKILL.md` with rich frontmatter (`name` ≤64, description, `allowed-tools`, `user-invocable`,
`disable-model-invocation`, `model`, `effort`, …); discovery across `.grok/skills|commands`,
repo root, home, **and `.claude`/`.cursor`/`.agents` compat paths**; slash + model-invoked with
scope-qualified collision names; `/create-skill` scaffolding. RapidLM: `SKILLS.md`, skills CLI,
`plugin-host::skills` — not model-callable.

**Plugins.** Claude: not evidenced. Grok Build: marketplace = git repo/local folder with
`.grok-plugin/marketplace.json` (pinned sha); a plugin bundles skills/commands/agents/hooks/MCP/LSP;
**enabled ≠ trusted** — hooks/MCP/LSP stay inert until trust; org rollout via managed config
(`xai-grok-plugin-marketplace`). RapidLM: plugin CLI with manifest validation, per-capability
approve/reject, hook dry-run, trust ledger — same philosophy, no marketplace.

**Hooks.** Grok Build: **15 events** — SessionStart, UserPromptSubmit (blocking: can reject a prompt),
PreToolUse (**deny or rewrite `updatedInput`**), PostToolUse, PostToolUseFailure, PermissionDenied, Stop
(**block stop**, 8-continuation cap/turn), StopFailure, StopCancelled (reason matchers), Notification,
SubagentStart, SubagentStop (blocking), PreCompact, PostCompact, SessionEnd; handlers `command` or `http`,
timeouts (5 s default, 600 s gates), regex matchers on tool/notification/subagent-type/error-class/cancel-
reason, fail-open on crash, run **before** permission rules; managed in a `/hooks` modal
(`xai-grok-hooks/src/event.rs`). Claude: `PreCompact` + `user-prompt-submit` evidenced only.
RapidLM: hook dry-run in plugin CLI; no in-loop hook events.

**Gap (P1, same pattern as tools):** the subsystems exist; the loop doesn't reach them. Wire order:
MCP first (with the never-dials refresh contract from Claude and the 20 KB cap + meta-tools from Grok
Build), then hooks (the blocking `PreToolUse`/`UserPromptSubmit`/`Stop` semantics are the valuable part),
then skills as `/<name>` + description-matched model invocation.

---

## 13. Sessions, durability & recovery

**Claude Code:** resume, transcript path surfaced in compaction, cross-session messaging
(ListAgents/SendMessage), remote cloud sessions (gated).

**Grok Build:** JSONL per session under `~/.grok/sessions/<encoded-cwd>/<id>/` — `chat_history.jsonl`,
`updates.jsonl` (authoritative ACP update stream; resume replays it), `rewind_points.jsonl`, `plan.md`,
`feedback.jsonl`, prompt snapshots, `compaction_checkpoints/`, `subagents/`; fs2 file locks; resume by
id/title or most-recent, `--fork-session` (fork filter strips incomplete tool turns), auto-titling;
**rewind via byte-level file snapshots captured before every operation** (not git), modes
All/ConversationOnly/FilesOnly, optional hunk-tracker deltas; **foreign-session import from Claude, Cursor,
Codex** (`xai-grok-foreign-sessions`).

**RapidLM:** event-ledger sessions with resume/fork/rewind/checkpoints, writer-lease recovery
(`inspect-export --recover`), ledger-quarantined cron, control-room projection, JSONL headless output
with typed exit codes.

**✓ Parity-or-ahead retained.** RapidLM's ledger is stronger than both references' JSONL/JSON stores
(replay, quarantine, leases). Two cheap adoptables: (a) fork filtering that strips incomplete tool turns
(Grok Build `fork_filter_chat`) — RapidLM's fork should already do this; verify; (b) `/rename` +
auto-titling and a `/export` conversation command are trivial UX wins. Remote/cloud sessions remain a
company-scale P3 in both references.

---

## 14. UX surface

| Area | Claude Code | Grok Build | RapidLM |
|---|---|---|---|
| Slash commands | Extensive set (not fully enumerated in evidence) | **~60 documented**, two sources (shell + TUI) fuzzy-unified; skills appear as commands; mode-gated sets | ~12 (`/goal /agents /models /memory /permissions /fork /compact /apply /control-return /quit`…) |
| Plan mode | 5-phase workflow, read-only mandate supersedes all | enter/exit tools; read-only **enforced in every mode incl. always-approve**; plan read from disk at exit; approve/request-changes/line-comment UI; state persisted across restarts | None |
| AskUserQuestion | 1–4 q × 2–4 opts, previews, auto "Other", AFK timeout | question cards, 1 800 s default wait, free-text, dismiss | None (`ApprovalRequired` unwired) |
| Themes/rendering | Not characterized | 5 themes + auto light/dark; markdown renderer with **Mermaid → PNG**; diff blocks w/ dual line numbers, fold/expand; vim mode; minimal/fullscreen render modes | TUI panels, color, layout; no theme engine |
| Queue/interject | Queued messages, brief mode | queue pane, cancel-and-send, draft stash, follow_up behavior config | Composer bounds only |
| Dashboards | — | `/dashboard` live agent roster; `/context` token breakdown; `/usage` billing; `/session-info` | Control-room projection (similar concept), goal panels |
| Background notifications | `<task-notification>` XML | per-task completion events + still-running status line + tasks pane | Supervisor events exist, not surfaced |
| Terminal support | Not characterized | 20+ terminals, `/doctor fix`, clipboard routing (OSC 52), Kitty keyboard protocol, notification matrix, voice dictation | Basic terminal handling |
| Import/compat | n/a | `/import-claude` (settings), foreign session import, `.claude`/`.cursor` compat everywhere | None |

**Gap (P2).** The two that matter for agent feel (unchanged): structured question cards (needs
`ApprovalRequired` wiring) and background-task notifications rendered into the transcript. Plan mode is
newly prominent as a P1-P2: both references gate destructive phases behind it, and Grok Build's
enforcement details (plan-file-only writes allowed, exit reads plan from disk, works even in
always-approve) are the spec to copy.

---

## 15. Headless, SDK & ACP (new section)

**Claude Code:** `-p` non-interactive mode evidenced; full flag/output-format surface **not captured** by
the case (thin — do not assert `--output-format` etc. from this evidence alone).

**Grok Build (the de-facto superset, Claude-compatible):** `-p` / `--prompt-json` / `--prompt-file`;
`--output-format plain|json|streaming-json|streaming-messages-json` — the last is the **Anthropic Messages
wire shape** (`system/init`, `assistant`, `user`, `result`); JSON result carries `text, stopReason,
sessionId, num_turns, usage (uncached + cache buckets), modelUsage per model with costUSD,
total_cost_usd (+ exact integer ticks at 10^10/USD), usage_is_incomplete`; streaming event types
text/thought/tool_call/tool_call_update/usage/plan/available_commands/end/error; flags `--tools`,
`--disallowed-tools` (incl. `Agent(explore)`), `--max-turns`, `--json-schema` (structured output),
`--allow/--deny`, `--sandbox`, `--effort`, `--worktree`, `--no-subagents`, `--system-prompt-override`;
exit codes 0/1/130/143. **ACP:** full `initialize`/`authenticate`/session lifecycle/`prompt`/`cancel`/
`set_session_mode|model`/ext-methods over stdio **and** WebSocket (`serve --bind --secret`); permission
prompts flow as ACP `session/request_permission`; client-type-aware UI (`xai-grok-shell/src/agent/
mvp_agent/acp_agent.rs`); official SDKs in five languages.

**RapidLM:** headless `exec` (answer-only stdout, JSONL, typed exits, `--verbose` diagnostics); `acp`
crate with v1/v2 and stdio + compat tests; kernel IPC/daemon.

**Gap (P2 → P1 for automation users).** RapidLM's typed-exit discipline is good; what's missing is a
**stable machine contract**: pick `streaming-json` (Grok shape) as the primary and, for compat, emit
Anthropic-shaped `streaming-messages-json`; add `--allow/--deny`, `--max-turns`, `--output-format`, and
usage/cost fields. The ACP crate should grow the `request_permission` + session-mode methods so IDE
hosts get the approval flow.

---

## 16. Auth, models & providers

**Claude Code:** billing/attribution header per request; model aliases sonnet/opus/haiku/fable; Fast mode
(Opus); haiku id `claude-haiku-4-5-20251001`; auth/telemetry specifics not in evidence.

**Grok Build:** auth modes WebLogin/OIDC/External-provider-binary/ApiKey; browser loopback OAuth2 **or**
RFC 8628 device-code flow, single-flight refresh; custom providers via `[model_providers.<id>]`
(base_url, env_key, api_backend responses|chat|messages, extra headers, context_window); model catalog
JSON (context_window 500 000, per-model reasoning-effort menus, auto-compact override, backend-search
support); effort levels low|medium|high|xhigh|max; per-model system-prompt labels; behavior-versioned
tools (`current` vs `legacy-0.4.10`).

**RapidLM:** `auth` crate (OS keychain, file keychain, mTLS, broker, local daemon, env identity) and
`llm-router` (catalog, credentials, **route filter/score/fallback chain**, phase routing, usage tracking)
with Anthropic + OpenAI-compatible providers.

**Assessment:** **✓ routing policy lead retained** (neither reference routes/falls back across providers;
Grok Build pins one provider with a circuit breaker). Gaps worth closing for parity: reasoning-effort
levels + per-model menus (P2), a model-catalog file with context windows driving compaction thresholds
(P1-adjacent — RapidLM's compaction needs a real context-window source), and device-code OAuth as a
headless-friendly login (P3).

---

## 17. Engineering quality

**RapidLM:** ~2 900 tests across 27 crates, `#![forbid(unsafe_code)]`, typed error enums, bounds as
consts, fail-closed defaults, inline test coverage, live-provider capture discipline. **✓ Lead.**

**Grok Build:** comparable discipline, visible in-source: per-crate tests incl. PTY harness and
plan-gate/permission/compaction tests, behavior versioning for tool changes, hermetic git test utils,
fail-closed flags (`usage_is_incomplete`), template encryption at rest, crash handler. Also the polish
budget of a shipped product (themes, mermaid, doctor, telemetry opt-outs).

**Claude Code:** not assessable (closed); hardened-runtime posture + unusually thorough prompt policy
engineering.

---

## 18. Complete-parity checklist (union of both references, RapidLM view)

`✓` has it · `~` partial/machinery-only · `✗` missing. "Parity target" = the union; where the references
differ, prefer the Grok Build contract (source-available, Rust, Claude-compatible).

**Re-audited 2026-08-29 against current HEAD.** This table (and §19) was written against commit `abe91b0`.
The very next commit after that was literally titled `ac66e8a Wire the gaps.md parity core into the exec
loop`, followed by ~15 more commits implementing large parts of this checklist and the §19 roadmap — but
neither table was ever refreshed. Five parallel code-reading passes re-verified every row against the live
`apps/rapid`/`crates` source (not commit messages). The **"RapidLM (orig.)"** column preserves what this
doc originally claimed; **"RapidLM (now)"** is the corrected, evidence-grounded verdict. Full evidence
(file:line citations) is in the audit notes below the table.

| Capability | Claude | Grok Build | RapidLM (orig.) | RapidLM (now) | Priority |
|---|---|---|---|---|---|
| Read tool (pagination, PDF/images, dedup) | ✓ | ✓ | ✗ | **~** — pagination/PDF/PNG done, dedup-on-unchanged unwired | **P2** |
| Grep/Glob search tools | ✓ | ✓ | ✗ | **~** — glob is real Claude-parity; "grep" is literal substring, no regex | **P1** |
| Exact-match edit tool | ✓ | ✓ | ✗ | **✓** | done |
| Shell tool (bg, timeouts, output caps) | ✓ | ✓ | ✗ | **✓** | done |
| Per-call tool-result messages | ✓ | ✓ | ✗ | **✓** | done |
| Parallel tool execution | ✓ | ✓ (locks) | ✗ | **✓** — `thread::scope`, per-path write-group locks, 16/step cap | done |
| Permission modes (6) + rules + grants | ✓ | ✓ | ✗ | **✓** | done |
| Kernel sandbox + profiles + auto-allow coupling | ~ | ✓ | ~ (unwired) | **~ (still unwired)** — separate ad-hoc macOS Seatbelt path, zero coupling to the permission lattice | **P1** |
| Subagents in-loop (general-purpose/explore/plan) | ✓ | ✓ | ✗ | **✓** — but see §19 item 8 (not background, no resume) | **P1** (residual) |
| TodoWrite-style task list tool | ✓ | ✓ | ✗ | **✓** | done |
| Plan mode (enter/exit, enforced read-only) | ✓ | ✓ | ✗ | **✓** — enforced at the permission-lattice level | done |
| Hooks in-loop (blocking PreToolUse/Prompt/Stop) | ~ | ✓ | ✗ | **~** — Pre/PostToolUse genuinely blocking; Prompt/Stop don't exist; a *separate*, richer 24-variant `HookEvent` enum in `plugin-host` is disconnected, reachable only via a dry-run CLI command | **P1** |
| MCP in-loop (+ OAuth, refresh, caps) | ✓ | ✓ | ~ (CLI only) | **~** — genuinely mid-turn callable now (no longer "CLI only"); no OAuth, no refresh, stdio-only | **P1** |
| Skills in-loop (SKILL.md, slash + model-invoked) | ✓ | ✓ | ~ | **~ (unchanged)** — full discover/activate machinery in `plugin-host`, zero references from `apps/rapid` | **P1** |
| AskUserQuestion cards | ✓ | ✓ | ✗ | **✓** — structured 2-8 option cards; single-question/single-select only (Claude's supports multi-question batching) | **P2** (residual) |
| Dynamic prompt sections / template engine | ✓ | ✓ | ✗ | **✓** — `PromptContext` (3 conditional sections: environment/trust/token budget) | done |
| AGENTS.md discovery + `.claude`/`.cursor` compat | ~ | ✓ | ✗ | **✓** — AGENTS.md + CLAUDE.md + `.claude/rules` + `.cursor/rules`, root→cwd hierarchy | done |
| Compaction auto-trigger + pruning + offload | ✓ | ✓ | ~ | **~ (unchanged)** — a real token-band `CompactionPolicy` exists but is never called; live path only compacts reactively on hard overflow; no pruning, no offload, no `/context` | **P2** |
| Memory index (always-loaded + typed records) | ✓ | ✓ | ~ | **~ (unchanged)** — still a naive `fs::read_to_string`; the 1,640-line typed `context_engine::memory` module has zero callers | **P2** |
| Headless JSON/streaming contract + cost fields | ~ | ✓ | ~ | **~ (unchanged)** — `JsonlWriter`/`JsonlRecord` defined but never constructed outside their own tests; `rapid exec` only understands `--verbose`/`--help` | **P1** |
| ACP with permission + session-mode methods | ~ | ✓ | ~ (v1/v2) | **~ (unchanged)** — `request_permission` is real and wired; both v1 and v2 explicitly return `METHOD_NOT_FOUND` for `session/set_mode` | **P2** |
| Background notifications into transcript | ✓ | ✓ | ✗ | **✓** — `drain_notifications()` runs at the top of every turn-loop iteration, no polling needed | done |
| Web search/fetch tools | ✓ | ✓ | ✗ | **~** — `web_fetch` is real and SSRF-guarded (resolves + refuses private/loopback IPs); no `web_search` (search-engine query) tool exists | **P2** |
| Scheduler/loop tools (model-callable) | ✓ | ✓ | ~ (CLI cron) | **~ (unchanged)** — still only reachable via `rapid cron`, no tool constant | **P2** |
| Goal/evidence in-loop (`update_goal`) | ✗ | ✓ | ~ (CLI only) | **✗** — no tool constant at all, not even partially reachable mid-turn | **P2** — differentiator |
| Ledger-durable cron + quarantine | ✗ | ✗ | **✓** | **✓ (confirmed)** | keep |
| Post-compaction verification | ✗ | ✗ | **✓** | **~ (downgrade)** — the fail-closed verify/evidence machinery in `compact_policy::compact_with_policy` is real and tested but has **zero callers**; the live overflow-recovery path calls raw `compact_packet` directly, bypassing it entirely | fix before claiming "keep" |
| Multi-provider routing/fallback | ✗ | ✗ | **✓** | **~ (downgrade)** — `llm-router::fallback::FallbackController` (1,468 lines, real retry/backoff/alternates) is re-exported but has **zero call sites**; `apps/rapid/src/model.rs` never imports it | fix before claiming "keep" |
| Computer use (browser/desktop/mobile) | ✗ | ~ (hub crates) | ~ (unwired crates) | **~ (unchanged)** — still no `ToolDriver` registration, no presence in `exec_tools.rs` | differentiator (unrealized) |
| Media generation tools | ✗ | ✓ | ✗ | **✗ (confirmed)** | P3/optional |
| REPL tool, Projects RAG, team memory, remote sessions | ✓/~ | ✗/~ | ✗ | **✗ (confirmed)** | P3/optional |

**The two "downgrades" matter more than the many upgrades.** Both `post-compaction verification` and
`multi-provider routing/fallback` were the doc's own headline evidence for RapidLM leading *both*
references (see the executive summary and §6's compaction assessment) — and both turn out to be real,
tested, orphaned machinery that the live turn loop never calls. That's a materially different risk profile
than "not built yet": the code exists, is presumably trusted because it's tested, and simply isn't in the
path that runs. Wiring `compact_with_policy` into `LiveRecoveryController::recover_from_overflow` and
`FallbackController` into `model.rs`'s provider-call path are both small, well-scoped fixes (the hard part —
the policy/fallback logic itself — is done) and should be prioritized ahead of new-feature roadmap items.

---

## 19. Remediation roadmap (revision 2)

Grok Build source paths are implementation references, cloned at `/Users/mohsin/grokbuild`. **Status
column added 2026-08-29** — re-verified against current HEAD; see the audit notes after §18 for the
methodology. `✓` genuinely wired into the live turn loop, `~` partial/orphaned, `✗` still open.

| # | Item | Sev | Effort | Status (2026-08-29) |
|---|---|---|---|---|
| 1 | `GatewayTool` → `ToolDriver` adapter; per-tool rollout | P0 | M | **✓** — `ToolDriver` implemented by `WorkspaceTools`/`ExecTools`, live-constructed |
| 2 | `RepoRead` (offset/limit, token cap) + `RepoSearch` (rg flags, head_limit/offset) | P0 | M | **~** — offset/limit real; cap is bytes not tokens; search has no regex/rg flags, literal substring only |
| 3 | Per-call tool-result messages in canonical stream | P0 | S–M | **✓** |
| 4 | `WorkspacePatch` exact-match edit | P1 | S | **✓** — must-differ + `replace_all` both enforced |
| 5 | Parallel dispatch + per-path write locks | P1 | M | **✓** — `thread::scope`, per-path write-group keys, 16/step cap |
| 6 | Permission lattice: 6 modes, `Tool(glob)` rules, remembered grants, read-only auto-allow | P1 | L | **✓** — including read-only tools auto-allowed before mode/grants are consulted |
| 7 | Sandbox profiles + approval coupling | P1 | M | **~** — `crates/sandbox` cataloged, only consumed by `rapid doctor`; live `shell_exec` sandboxing is a separate macOS-only Seatbelt path, zero coupling to permissions |
| 8 | Subagents in-loop; adopt general-purpose/explore/plan names; bg default; resume; depth 1 | P1 | M | **~** — naming/in-loop/depth-1 done; runs **synchronously, blocks the parent turn** (no background default, no resume); `PersistentSpecialist` machinery is unreferenced dead code |
| 9 | MCP in-loop: meta-tools + 20 KB cap + never-dials refresh | P1 | M | **~** — cap confirmed (20 KiB); no meta-tools (search_tool/use_tool) pattern; `catalog::refresh` exists but is never called |
| 10 | Hooks in-loop: event enum + blocking semantics | P1 | M | **~** — blocking semantics real for Pre/PostToolUse; the 24-variant typed event enum this item asks for exists in `plugin-host` but is a disconnected system, reachable only via a dry-run CLI test command |
| 11 | Prompt template engine + PromptContext (env/trust/token/cause sections; subagent variant; post-compact short prompt) | P1 | M | **✓** — `PromptContext` by that exact name, wired for both parent and subagent turns |
| 12 | AGENTS.md/rules discovery + `.claude`/`.cursor` compat paths | P2 | S | **✓** |
| 13 | Compaction: auto-trigger %, keep-last-N tool-result pruning, segment offload, `/context` breakdown | P2 | M | **~ (unchanged)** — none of the four sub-items exist in the live path |
| 14 | Plan mode + todo tool + question cards in TUI | P2 | M | **~** — all three tools are live-wired in the exec loop; zero TUI rendering exists for any of them |
| 15 | Memory index: always-loaded bounded pointer + typed records + origin-keyed workspace memory | P2 | S | **~ (unchanged)** — still a naive file read; the typed module has zero callers |
| 16 | Headless contract: `--output-format json\|streaming-json(+messages)`, `--allow/--deny`, `--max-turns`, usage/cost fields | P1 | M | **~ (unchanged)** — the JSONL types exist but nothing outside their own tests constructs them; no flags |
| 17 | ACP: `request_permission` + session modes | P2 | M | **~ (unchanged)** — permission real; `session/set_mode` explicitly returns `METHOD_NOT_FOUND` in both v1 and v2 |
| 18 | `update_goal` + `EvidenceRecord` in-loop (model cites verification) | P2 | S | **✗** — no tool constant exists anywhere; goal lifecycle is CLI-only |
| 19 | SDK-style tool-schema artifact export | P2 | S | **~ leaning ✗** — a versioned, hashed `ToolCatalog` exists but is consumed only internally; no export command or generated artifact |
| 20 | Model catalog file (context windows, effort menus) feeding compaction + router | P2 | S | **✗** — `llm-router::catalog` is real but models pricing/capability/region, not context-window size; `DEFAULT_CONTEXT_WINDOW` is still one hardcoded constant |

**Sequencing logic (updated 2026-08-29):** items 1, 3, 4, 5, 6, 11, 12 are now done — the "unlock" and the
safety layer (6) both landed. What's left clusters into three real categories, not the original 1→20
priority order: **(a) orphaned machinery** — 7 (sandbox), 9 (MCP refresh/meta-tools), 10 (hook event enum),
15 (memory), 19 (schema export), and the two §18 "keep" downgrades (post-compaction verification,
provider fallback) — these are the cheapest wins in the whole document, since the hard logic already
exists and is tested; only the wiring is missing. **(b) genuinely unbuilt:** 13 (compaction ergonomics), 16
(headless contract), 17 (ACP session modes), 18 (`update_goal`), 20 (model catalog). **(c) backend-done,
UI-only:** 14 (plan/todo/question-card TUI rendering) — the exec-loop tools all work headlessly today; only
the terminal UI surface is missing.

---

## 20. External findings translated onto RapidLM (2026-08-29)

Twelve findings from a third-party product review (the reference product's own component IDs — worktree
pools, a checkpoint manager, an Attempt-Step state machine, a Message Phase Router, a Completion Gate, a
`modbit-index` scope graph — do not exist in this codebase and are dropped below) re-grounded against
RapidLM's actual crates. Several land on primitives that already exist but aren't wired for this exact
purpose; a few land on something RapidLM already does *better* than the finding assumed. Verified by
reading the cited source, not inferred from crate names.

| # | Finding | RapidLM basis today | Gap / what's missing |
|---|---|---|---|
| 1 | **Shadow Workspace** — verify a candidate edit before ever showing it, not after applying | `workspace::backends::git_worktree` ("Isolated Git worktrees for **write-capable subagents**" — already exists for exactly this isolation shape) + `overlay` ("writes never mutate the parent tree") + `workspace::checkpoint::CheckpointManager` (persisted view checkpoints/rewind) | **Implemented 2026-08-29.** `apps/rapid/src/shadow_diagnostics.rs` (new module) wires `git_worktree` into `workspace_write`: opt-in via `.rapidlm/settings.json`'s `shadow_diagnostics: {command, globs}`, materializes the candidate content into an isolated worktree, runs the configured command there, and only applies to the real tree on success — a failing candidate never reaches disk or reads back as applied. Fails open (falls back to a direct write) when the root isn't a Git repo or worktree setup fails, since this is a quality feature, not a security boundary. `workspace_patch` is not covered yet (same mechanism, needs the pre-apply computed content threaded through its call site) — noted as a follow-up, not silently half-done. 10 tests (`shadow_diagnostics::tests`, `exec_tools::tests::shadow_diagnostics_*`), including a regression test for a worktree-leak bug found during implementation (the candidate write always dirties the worktree, and `remove_view` never force-deletes a dirty one by design — fixed by discarding the worktree's own changes before removal). |
| 2 | **Task pause as a distinct state** — suspend a running step without killing its process group, resumable, neither cancel nor steering | `scheduler::kinds::NodeState`: `Pending, Ready, Running, Waiting, Blocked, Succeeded, Failed, Cancelled, Superseded, Invalidated` (10 states) | **Implemented 2026-08-29** at the graph-state-machine layer (the layer this finding is actually about). `NodeState::Paused` added; `GraphService::pause`/`resume` (`crates/scheduler/src/service.rs`) mirror the existing `cancel_tree`/`wait`/`resume_wait` ledger-first pattern — `pause` only from `Running`, `resume` goes straight back to `Running` (not `Pending`: unlike a waited node, a paused one was already mid-work and re-checking dependencies would be wrong), both reject from any other state rather than no-op. `Paused` is deliberately non-terminal, so `cancel_tree` still cascades into a paused descendant. **Not included**: actual OS-level process suspension (SIGSTOP/SIGCONT) for `NodeKind::Process` nodes — `process-supervisor` has no such primitive today, and wiring the graph state to a real process signal is a separate, larger piece of work than the state machine gap identified here. No `rapid graph` CLI surface exists yet to expose this either (the `rapid graph show\|watch\|diff\|why-ready\|why-blocked\|retry\|export` surface listed in `00-README.md` is aspirational — `rapid graph` isn't wired into the CLI dispatch at all, a pre-existing gap this pass didn't take on). 2 new tests in `crates/scheduler/src/lib.rs`. |
| 3 | **Steering vocabulary** as a 3-value enum (queue / steer / interrupt) | Nothing. `grep -rln "steer"` across the whole tree returns zero hits. | RapidLM has no mid-turn steering mechanism at all today — `ask_user` is the model *pausing to ask*, not the user *injecting guidance into a running turn*. The 6-mode permission lattice (`default\|plan\|acceptEdits\|auto\|dontAsk\|bypassPermissions`, §4) is orthogonal — it gates approval, not turn control. An evidence/audit-first posture (goal completion gated on verifier verdicts, §11) argues for defaulting new steering input to *queue*, not *steer*, when this gets built. |
| 4 | **Two-tier autonomy**: supervised foreground + a background, config-gated "reviewable suggestions only, never auto-applied" tier | `security::scanners::secrets::Finding` — a working Finding-lifecycle type already exists for one kind of background scan | Not generalized. `rapid cron` (`scheduler::cron`, `event-ledger::cron`, claim-lease firing) runs durable background prompts, but nothing distinguishes "propose, never apply" from full execution as a config-gated mode. The generalization is: reuse the `Finding` shape for other background automations, not invent a new one. |
| 5 | **Anti-hallucination verification stage** — check a proposed edit/call references real, resolvable symbols, gated on AST/symbol context (not lexical search) | `context_engine::index::graph::CodeGraph`, already wired into scouting (`context_engine::scout::ScoutSources::graph()`, `ContextScoutReport`) | `CodeGraph` exists and is reachable from a scout report, but nothing in the tool-call validation path (`apps/rapid/src/exec_tools.rs`) checks a proposed call against it before dispatch. Sequencing signal carries over unchanged: build this only once `CodeGraph` is confirmed populated and fresh for the target repo, not stale/best-effort — the same ordering dependency the original finding named, just against RapidLM's own graph instead of a foreign one. |
| 6 | **Plan model ≠ execution model**, with the routing decision recorded | `llm_router::phase::{ModelPurpose::Plan, PhaseRoute}` — phase-based routing to a distinct profile **already exists** ("phases without an override resolve to `main`") | This elaborates something partially built, not a new concept — but confirmed unreachable today: `apps/rapid/src/model.rs::build_request` hardcodes `ModelPurpose::Chat` as the only purpose ever sent (the single call site of `ModelPurpose::` in that file), so `PLAN_ENTER_TOOL`/`PLAN_EXIT_TOOL` turns (`exec_tools.rs`) never route through `ModelPurpose::Plan` regardless of `PhaseRoute` configuration — any override set there is dead code from `rapid exec`'s side. Fix is two parts: thread the live plan/exec phase into `build_request`'s purpose argument, then record the resolved `ProfileId` alongside the existing per-step diagnostics (`model step host=... tokens=...` lines, `apps/rapid/src/host.rs`). |
| 7 | **Second-opinion / adversarial verification before "done"**; disagreement becomes evidence, never auto-flips the verdict | Goal completion is already gated on "criteria + fresh evidence + **required verifier verdicts**" (V3 invariant #8, [00-README.md](00-README.md)); `Verification`/`Evidence` are first-class `NodeKind`s (`scheduler::kinds`); `rapid evidence show\|verify\|export` exists | The closest 1:1 fit of all twelve — RapidLM's own completion-gate equivalent already exists structurally. What's unconfirmed: whether the verdict model is genuinely tri-state (`VERIFIED\|REJECTED\|INDETERMINATE`) today, or closer to a boolean pass/fail — worth checking `event-ledger`/`harness`'s actual verdict type before assuming the shape needs to change at all. |
| 8 | **Capability-narrowing for remediation/child runs** — a child run authorized by a single-use, scope-limited token, never inheriting the parent's authority unchanged | `agent_runtime::specialist::PersistentSpecialist::inherits_parent_lease()` is **hardcoded `false`** ("specialists never hold an ambient parent capability lease"); `capability_broker::lease::{LeaseIssuer, CapabilityLease}` + `validator::{LeaseUseGuard, ConsumedLeaseUse}` already model single-use, validated lease consumption | Nuanced, not a clean gap: for **read-only** specialists this exact principle is already enforced at the type level — ahead of the original finding. But `PersistentSpecialist::new()` rejects any non-read-only role outright (`NotReadOnly` error), so there is currently *no path at all* for a write-capable, narrowly-scoped remediation child run (e.g. "fix this one CVE in this one file"). The read-only case is solved; the write-scoped-narrow case doesn't exist yet — that's the real gap, and `LeaseIssuer`/`CapabilityLease` look like the right primitives to extend rather than a new mechanism. |
| 9 | **Inline run-summary card** in a Timeline-style surface: collapsed `Worked for 4m 13s +25 −131` row, rendered from reference/count data only, never the diff body | `crates/tui/src/panels/trace_jobs.rs` (`TraceJobsViewModel`) — already a frontend projection over kernel job/process traces, already redacts secrets, already bounds log pages via `ArtifactCursor` rather than rendering raw bodies | No collapsed summary row exists yet, but the "reference/count data only" principle the finding names is already this panel's own stated design rule — it would be an additive rendering change on an existing, already-compatible data model, not new plumbing. |
| 10 | **Skill/playbook attribution chip** per step ("Used playbook: X"), sourced from compile-time data, attribution only, grants no capability | `scheduler::playbook::{PlaybookTemplate, compile}` (wired in `apps/rapid/src/p9_commands.rs`) + `plugin-host::skills` — selection already happens at compile time | Same shape as #9: nothing surfaces which playbook/skill produced a given step in the TUI today; low-risk, additive once a Timeline-like surface exists to attach it to. |
| 11 | **Guardian** — an AI approval-reviewer subagent that narrows toward human review for high-risk actions, explicitly never a full replacement, no ceiling recommended without one | `capability_broker::approval::{ApprovalChoice::{Deny, Approve}, ApprovalResolution, request_approval}` — the approve/deny primitive is already principal-agnostic; nothing at the type level restricts resolution to a human `PrincipalRef` specifically | Only a human (or policy auto-resolution under `bypassPermissions`/similar modes) resolves an `ApprovalChoice` today; no AI-judged path exists. This directly validates RapidLM's own already-planned "model-judged approval mode" and gives it a concrete typed hook to land on (`ApprovalChoice`/`request_approval`) instead of inventing a parallel mechanism — with the same caveat the original finding raised: no ceiling has been proposed here either, and one is needed before this ships. |
| 12 | **`node_repl`-style persistent code-execution runtime** | `sandbox` crate exists (`backend.rs`, `backends/`) but is scoped to bounded, one-shot `shell_exec` calls; `process-supervisor` manages long-lived processes generally, but nothing wires a persistent interpreter session into the model-facing tool surface | Not proposed as buildable here either. Worth recording that RapidLM hasn't greenlit this surface any more than the reference product had — it's a real capability/security-surface decision, not a small addition, whichever codebase considers it first. |

---

## Method note & limitations

Grok Build findings are grounded in the open-source tree at `9684fa3` and cite file paths; behavior of the
*shipped* binary may differ from this sync (check `SOURCE_REV`). Claude Code findings derive from v2.1.220
embedded prompts and SDK type definitions recovered in the cited case workspace; implementation-level
behavior (exact limits, retry policy, CLI flags, settings hierarchy, MCP transports/auth, sandbox
internals, cost tracking) may differ and is flagged as thin rather than asserted. RapidLM findings are
grounded in the tree at `abe91b0` and this workspace's docs. Claude-side quotes remain authoritative in the
case evidence files; Grok-side claims are verifiable directly in the cloned tree.
