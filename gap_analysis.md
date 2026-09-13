# RapidLM CLI vs Qwen Code, Auggie (Augment Code), and Kimi CLI — Gap Analysis

- **Date:** 2026-09-01
- **RapidLM side:** this workspace @ `ad54349` (main), Rust workspace, 28 crates + 1 binary,
  281,220 lines of Rust, 3,135 tests, `#![forbid(unsafe_code)]`. `cargo check --workspace
  --all-targets` is **clean** (warnings only). Every RapidLM claim below is grounded in
  source at a cited path, and the behavioural claims were **executed against a freshly built
  `target/debug/rapid`**, not inferred.
- **Qwen Code side:** `QwenLM/qwen-code` @ `431ff3a` (2026-09-01), **v0.22.3**. TypeScript
  monorepo, 20 packages. `packages/core` + `packages/cli` alone are ~788k non-test lines;
  2,122 test files. Full source inspected.
- **Auggie side:** `@augmentcode/auggie` **0.35.0** at `/Users/mohsin/aug/package` — a single
  13 MB minified/bundled `augment.mjs`. Claims are grounded in strings and structures
  recovered from that bundle; **confidence is lower than for the two open-source
  references** and is flagged per-claim.
- **Kimi CLI side:** `MoonshotAI/kimi-cli` @ `86f1364` (2026-09-01), **v1.50.0**. Python,
  52k lines in `src/` plus 14k in vendored packages (`kosong`, `kaos`), 2,762 test
  functions. Full source inspected. Note: upstream has announced this project is being
  wound down in favour of `MoonshotAI/kimi-code`; it remains the best available proxy for
  Moonshot's agent design.
- **Severity:** `P0` = RapidLM is not a usable product without this · `P1` = major
  competitive gap · `P2` = polish/differentiator gap · `✓` = at parity or ahead.

---

## 1. Verdict

**RapidLM does not currently match any of the three references, and is not close to
surpassing them.** The gap is not primarily one of missing features in the abstract — the
repository contains a large, well-engineered, well-tested implementation of many of the
right primitives. The gap is that **the primary product surface does not work at all**, and
most of the built subsystems are not reachable from any shipped entry point.

Three findings dominate everything else in this document:

1. **The interactive TUI — `rapid` with no arguments, the flagship surface — never sends
   anything to a model.** `SubmitTurn` (`crates/kernel/src/client.rs:71-76`) carries
   `session_id`, `expected_seq`, `actor`, `trace_id` and **no prompt text**; the composer
   contents are dropped on the floor at `apps/rapid/src/interactive.rs:1560` (`submit_turn`
   reads nothing from `self.ui.composer()`). The `kernel` crate has **no dependency on
   `agent-runtime` or `llm-router`** (`crates/kernel/Cargo.toml`). The TUI is a durable
   event projection with a text box wired to nothing.
2. **Project trust can never be granted by any shipped command.**
   `ProjectTrustStore::set` (`crates/kernel/src/project/trust.rs:309`) is called from
   **tests only** — never from `apps/rapid/src`. Workspace tools are gated on
   `TrustStatus::Trusted` (`apps/rapid/src/interactive.rs:1563`), and `rapid exec`'s own
   error message tells the user to "approve trust by running `rapid` interactively once"
   — which does nothing, because the TUI has no trust-grant path. Verified live: in a fresh
   git repo, `rapid exec "say hi"` reaches a real model but prints
   `warning: workspace tools are disabled for this run: the project is not trusted`.
   **Out of the box, the agent cannot read or write a single file.**
3. **`rapid --help` advertises 28 subcommands; 15 exist, and 6 of those aren't advertised.**
   Executed against the built binary: `run`, `resume`, `fork`, `rewind`, `daemon`, `acp`,
   `graph`, `context`, `evidence`, `trace`, `process`, `computer`, `sandbox`, `mcp`,
   `hooks`, `skills`, `eval`, `inspect`, `export`, `update` **all print top-level usage and
   exit 2**. The real command set, from `rapid completions bash`, is: `exec goal
   playbook-compile mcp-tools tools agent-cli sessions inspect-export cron findings agents
   plugins doctor completions man` (+ `insights`, `release-manifest`, undocumented).

What *does* work is `rapid exec` — a genuinely functional headless agent turn with 15
tools, a six-mode permission lattice, parallel tool dispatch, per-call `tool_result`
messages, AGENTS.md loading, retrieval, todos, reminders, memory index, hooks and stdio
MCP. That is a solid single-turn headless agent. It is roughly **one surface out of the
seven** each reference ships, with **15 tools against Qwen Code's 47**, and it is the only
part of the product that runs.

---

## 2. Reference snapshot

| | Qwen Code 0.22.3 | Auggie 0.35.0 | Kimi CLI 1.50.0 | RapidLM `ad54349` |
|---|---|---|---|---|
| Language / runtime | TypeScript / Node 22+ | TypeScript (bundled) / Node 20+ | Python 3.12+ | Rust 2024 |
| Working interactive TUI | ✅ | ✅ | ✅ | ❌ **no model call** |
| Working headless mode | ✅ `-p` | ✅ `--print` | ✅ `--print` | ✅ `rapid exec` |
| Model-callable tools | **47** built-in | **~30** local+sidecar+remote | **19** (+ subagent-scoped) | **15** |
| Subagents | ✅ file-defined + built-in | ✅ `sub-agent`, parallel | ✅ YAML-defined, 3 built-in | ⚠️ 2 built-in, non-extensible |
| Agent teams / multi-agent | ✅ `team_create`, mailbox, plan approval | ✅ resident/cloud agents, pools | ❌ | ❌ |
| Declarative workflows | ✅ JS-dialect `Workflow` tool | ❌ | ✅ Flow skills (mermaid/D2 graphs) | ❌ (`playbook-compile` CLI only) |
| Skills | ✅ 12 bundled + 22 repo + curator + auto-activation | ✅ `/skills` | ✅ layered discovery, flow skills | ⚠️ 2,307 LOC, **never reaches the model** |
| Hook events | **22** (command/HTTP/prompt/function, async, `if`) | tool-call/tool-response + webhook/script policies | **13** (command, matcher regex) | **6** (command, **no matcher**) |
| Permission modes | 6 + LLM auto-classifier | allow/deny/ask + webhook + script policy, per-tool regex | yolo / afk / plan / approve-for-session | 6 (no classifier) |
| MCP | client (stdio+HTTP+SSE), OAuth, pooling, resources, registry | client + `auggie mcp` + marketplace | client (stdio+HTTP), OAuth, `kimi mcp` | ⚠️ **stdio only, no OAuth**, cap 8 servers |
| ACP | ✅ `acp-bridge` + `qwen serve` (HTTP+SSE) | ✅ `--acp` | ✅ `kimi acp` | ❌ crate exists, **no `rapid acp`** |
| IDE integrations | VS Code, JetBrains, Zed, Chrome ext. | VS Code, JetBrains, Slack | VS Code, Zed, JetBrains (via ACP) | ❌ |
| GUI surfaces | Desktop app, Web UI, IM channels (TG/DingTalk/WeChat/Feishu) | Desktop, Slack, cloud | Web UI (`/web`), tracing visualizer (`/vis`) | ❌ |
| SDKs | TypeScript, Python, Java | (CLI-as-API) | Python + wire protocol | ⚠️ TS SDK exists, **no server to talk to** |
| Providers | OpenAI, Anthropic, Gemini, Qwen, any OAI-compatible, Ollama/vLLM | Augment cloud | Kimi + any OAI-compatible | OpenAI-compatible + Anthropic |
| Auth | OAuth device flow + API keys, `/auth` | `auggie login` OAuth | `/login` browser OAuth, keyring | **API key in TOML only** |
| Prompt caching | ✅ | ✅ (server-side) | ✅ | ❌ `caching: false` hard-coded |
| Vision / image input | ✅ | ✅ `--image` | ✅ `ReadMediaFile` | ❌ `vision: false` hard-coded |
| Context budget | model-derived, compression + `/compact` | server-side context engine | model-derived + auto/manual compact | ❌ **hard-coded 8,192 tokens** |
| Self-update | ✅ installer + `/update` | ✅ npm | ✅ `/upgrade` | ❌ |
| Distribution | npm, brew, standalone installer, Docker | npm | PyPI, standalone binary, Nix | ❌ **source build only** |
| i18n | 7 languages | — | EN + ZH docs | ❌ |

---

## 3. P0 — Blockers

### 3.1 The interactive TUI cannot run an agent turn (P0)

**Evidence.** `apps/rapid/src/interactive.rs:1560-1575`:

```rust
fn submit_turn(&mut self) -> Result<(), InteractiveError> {
    if self.ui.actions_blocked() { return Ok(()); }
    let expected_seq = self.ui.snapshot().map(|s| s.seq()).unwrap_or(0);
    block_on(self.client.submit_turn(SubmitTurn::new(
        self.session_id, expected_seq, self.actor.clone(), TraceId::new(),
    )), self.cancel)?;
    self.drain()
}
```

The user's text is read at `submit_composer` only to test `starts_with('/')`, then discarded.
`SubmitTurn` has no prompt field. `crates/kernel/Cargo.toml` depends on `protocol`,
`event-ledger`, `auth`, `serde`, `serde_json`, `toml` — **no `agent-runtime`, no
`llm-router`**. There is no code path from a keystroke in the TUI to a model request.

**What the references do.** All three run the same agent core in interactive and headless
mode. Qwen Code: `packages/core/src/agents/runtime/agent-core.ts` with
`agent-interactive.ts` / `agent-headless.ts` as thin frontends. Kimi CLI: `KimiCLI` in
`src/kimi_cli/app.py` drives one `Soul` regardless of shell/print/ACP/wire frontend.

**Fix.** Add `prompt: String` to `SubmitTurn`, and have the kernel turn service delegate to
the same `AgentExecutor` + `LiveContextHost` composition `exec_turn` already builds
(`apps/rapid/src/interactive.rs:1539+`). This is the single highest-value change in the
repository: it converts ~250k lines of infrastructure from "unreachable" to "shipping".

### 3.2 Project trust is un-grantable (P0)

`ProjectTrustStore::set` is dead outside tests. Consequences, verified live:

- `rapid exec` runs with **read-only-nothing**: no `workspace_read`, `repo_read`,
  `repo_search`, `repo_glob`, `workspace_write`, `workspace_patch`, `shell_exec`.
- Proactive context retrieval is skipped entirely (`interactive.rs:1594`, gated on
  `TrustStatus::Trusted`).
- Executable project config is inert (`executable_config_active: trust.is_trusted()`).

**Fix.** Either (a) a first-run trust prompt in the TUI writing through
`ProjectTrustStore::set`, or (b) a `rapid trust grant|revoke|status` subcommand — the
references do both (`/trust` in Qwen Code, first-run dialog in Kimi and Auggie).

### 3.3 The advertised CLI is largely fiction (P0 for credibility)

`CLI_USAGE` (`apps/rapid/src/interactive.rs:193-222`) documents a 28-command surface
described in the README as the "Primary CLI surface". `run_subcommand`
(`interactive.rs:325-345`) dispatches 17 arms, 6 of which are undocumented internals
(`playbook-compile`, `mcp-tools`, `tools`, `agent-cli`, `inspect-export`,
`release-manifest`).

Executed result — **20 advertised commands exit 2 with top-level usage**: `run`, `resume`,
`fork`, `rewind`, `daemon`, `acp`, `graph`, `context`, `evidence`, `trace`, `process`,
`computer`, `sandbox`, `mcp`, `hooks`, `skills`, `eval`, `inspect`, `export`, `update`.

**Fix.** Either implement them or cut `CLI_USAGE` down to what exists and move the target
surface into a roadmap document. Shipping a help text that is two-thirds wrong is worse
than shipping a small CLI. Note the existing test at `interactive.rs:3209` asserts
`CLI_USAGE.contains(cmd)` — it validates the *documentation*, not the dispatch, and so
passes while the commands are absent.

### 3.4 Context budget is hard-coded to 8,192 tokens (P0)

`apps/rapid/src/interactive.rs:1569-1576` calls
`build_live_context(..., context_limit = 8192, output_reserve = 256)`. The user config
*parses* `context_window` (`apps/rapid/src/user_config.rs:494`) and it reaches the model
descriptor (`apps/rapid/src/model.rs:161-164`), but **never reaches the context compiler**.
Every turn is compiled against an 8k budget with a 256-token output reserve regardless of
whether the configured model has a 200k window.

Practical effect: retrieval blocks, AGENTS.md, memory index, todos and reminders compete
for ~8k tokens, and compaction fires far earlier than it should. This alone would cap
RapidLM's task success rate well below all three references at a fixed model.

**Fix.** Thread `ActiveModel::context_limit` / `max_output` into `build_live_context`.

### 3.5 The SDK has no server to connect to (P0 for the SDK)

`sdk/typescript/src/client.ts:252` sends `prompt` in a `turns.submit` RPC over a Unix
socket / loopback websocket (`src/transport/local.ts`). An `IpcServer` exists
(`crates/kernel/src/ipc/server.rs:322`), but **no CLI command binds it** — `rapid daemon`
is unimplemented (§3.3). And even if bound, the kernel would drop the prompt (§3.1). The
6,103-line SDK cannot complete a single task today.

Qwen Code ships three working SDKs (TS/Python/Java) plus `qwen serve` (HTTP+SSE, multi-
client). Kimi ships a Python SDK plus the `wire` JSON-RPC protocol and `kimi acp`.

### 3.6 `rapid doctor` is a stub (P1, listed here because it hides the above)

`run_doctor` (`apps/rapid/src/p9_commands.rs`) calls
`security::evaluate_doctor(&DoctorRequest::default(), …)` — a request with no gathered
inputs. Live output, in this very repository, on macOS with `sandbox-exec` present:

```
sandbox_availability Unavailable
policy_parse Unavailable
credential_store Unavailable
dangerous_project_config Unavailable
release_signature Unavailable
```

Every check is `Unavailable` because nothing populates the request. Qwen Code's
`doctorChecks.ts` and Kimi's `/debug` + `kimi info` report real environment state.

---

## 4. P1 — Capability gaps by dimension

### 4.1 Model-callable tool surface

RapidLM ships **15** tools (`rapid tools`, verified):
`workspace_write`, `workspace_read`, `repo_read`, `repo_search`, `workspace_patch`,
`repo_glob`, `todo_write`, `plan_enter`, `plan_exit`, `job_status`, `job_output`,
`task_spawn`, `ask_user`, `web_fetch`, `shell_exec`, plus dynamic `mcp__<server>__<tool>`.

Qwen Code ships **47** (`packages/core/src/tools/tool-names.ts`). Missing from RapidLM,
grouped by what the absence costs:

| Missing tool | Reference | Cost of absence |
|---|---|---|
| `web_search` | Q, A, K | The agent cannot find anything it doesn't already have a URL for. All three references have it; RapidLM has fetch-only. |
| `save_memory` / `remember` | Q, A | No agent-writable memory. RapidLM reads `.rapidlm/MEMORY.md` but nothing can write it. |
| `skill` | Q, K | Skills are unreachable (§4.4). |
| `lsp` | Q | No go-to-definition/references/diagnostics for the model. RapidLM *has* an LSP client (`crates/context-engine/src/lsp/`) — unwired. |
| `notebook_edit` | Q | Jupyter workflows impossible. |
| `zoom_image`, `display_image`, `read_media` | Q, K | No image reasoning loop. |
| `image_gen` | Q | — |
| `ask_user_question` (structured multiple-choice) | Q, K | RapidLM's `ask_user` is free-text only. |
| `monitor` | Q | No event-driven waiting on external state. |
| `tool_search` (deferred tool schemas) | Q | Cannot scale past ~50 tools without blowing the prompt. |
| `read_mcp_resource` | Q | MCP resources unusable. |
| `enter_worktree` / `exit_worktree` | Q | RapidLM has `GitWorktreeStore` — unwired to the model. |
| `workflow` | Q | — |
| `team_create` / `send_message` / `team_plan_approval` | Q | No multi-agent. |
| `cron_create/list/delete`, `loop_wakeup` | Q | RapidLM has a durable cron with claim-leases — **CLI-only, invisible to the model**. |
| `task_create/update/list/stop`, `create_sub_session` | Q, K | RapidLM's `job_status`/`job_output` cover a strict subset. |
| `report_findings`, `record_artifact`, `artifact` | Q | RapidLM has a findings store — CLI-only. |
| `get_goal` / `update_goal` / `propose_goal` | Q | **RapidLM's deepest subsystem is CLI-only.** The model cannot see or move its own goal. |
| `evidence.record` | (RapidLM's own catalog) | Defined in `GatewayTool` — unwired. |
| `git-commit-retrieval`, `conversation-retrieval` | A | — |
| `read-terminal`, `open-browser`, `render-mermaid` | A | — |
| `diagnostics` | A | RapidLM has `shadow_diagnostics.rs` — internal only. |

There is also a **second, orphaned catalog**. `crates/tool-gateway` (5,270 lines) is a
workspace member that **no other crate depends on** — it appears in no `Cargo.toml` besides
its own. Inside it, `GatewayTool` (`src/schema.rs:213`) defines 12 tools (`repo.search`, `repo.read`,
`workspace.patch`, `workspace.status`, `shell.exec`, `agent.spawn`, `agent.result`,
`goal.update`, `browser.act`, `mobile.act`, `external.call`, `evidence.record`) with
dot-separated names, schemas, validation and repair logic — **none of which the exec loop
uses**. `apps/rapid/src/exec_tools.rs` defines its own 15 with underscore names. Two tool
authorities, one wired. This is exactly the "parallel authority" the README's own
migration rule forbids.

**Fix, in order:** `web_search` → `save_memory` → `skill` → `goal_update`/`goal_get` →
`lsp` → `tool_search` → structured `ask_user_question` → worktrees. Then delete or
promote `GatewayTool` — do not keep both.

### 4.2 Tool-call mechanics ✓ (mostly at parity)

RapidLM is **at parity** here, and this is worth stating plainly because an earlier
internal analysis (`gaps.md`) said otherwise and is now stale:

- Per-call `tool_result` messages with `tool_call_id` and `MessageRole::Tool`:
  ✅ (`apps/rapid/src/model.rs:448`, `crates/llm-router/src/provider.rs:1248-1260`).
- Parallel dispatch with read/write classification and per-path write serialization: ✅
  (`tool_kind` / `tool_class`, `apps/rapid/src/exec_tools.rs:2363-2381`; `max_parallel` /
  `max_write_parallel` in `crates/agent-runtime/src/agent/scheduler.rs:75`).
- Streaming required on both provider paths: ✅ (`apps/rapid/src/model.rs:212,227`).

**Remaining gaps:** no tool-result image content (Qwen and Kimi both return images from
tools); no deferred/searchable tool schemas; no tool-result retention/truncation policy
comparable to Qwen's `tool-result-retention.ts` + `truncation.ts`.

### 4.3 Agent orchestration

| | Qwen Code | Auggie | Kimi CLI | RapidLM |
|---|---|---|---|---|
| Subagent definitions from files | `.qwen/agents/*.md` frontmatter | `--agent <id>`, personas | `--agent-file` YAML, per-agent `allowed_tools`/`exclude_tools` | ❌ 2 built-ins (`explore`, `patch`), `AGENT_TYPES` const |
| Parallel subagents | ✅ | ✅ explicit "can be run in parallel" | ✅ | ❌ "one subagent at a time" (`exec_tools.rs:588`) |
| Teams / mailboxes | ✅ `TeamManager`, `mailbox.ts`, plan approval, model routing | ✅ resident agents, pools, Slack | ❌ | ❌ |
| Declarative workflows | ✅ `workflow-orchestrator.ts`, budget, journal, stall detection, resume | ❌ | ✅ Flow skills (mermaid/D2, `<choice>` branch nodes) | ⚠️ `playbook-compile` CLI, not model-reachable |
| Background tasks | ✅ `background-tasks.ts` + resume | ✅ async subagents | ✅ `/task` browser, worker process, auto-notify | ⚠️ `shell_exec background=true` + `job_status`/`job_output` only |
| Agent arena (N models, same task) | ✅ `ArenaManager` | — | — | ❌ |
| Terminal backends (tmux/iTerm) | ✅ | — | — | ❌ |

RapidLM's `agent_defs` loader (`crates/agent-runtime/src/agent_defs/`) already parses
project agent definitions from `.rapidlm/agents` and `rapid agents list` exercises it — but
`task_spawn`'s `AGENT_TYPES` is a hard-coded `&["general-purpose", "explore", "plan"]`
(`exec_tools.rs:95`), so project-defined agents are listable and unusable.

### 4.4 Skills — built, never delivered

`crates/plugin-host/src/skills.rs` is 2,307 lines of `SKILL.md` discovery, frontmatter
parsing, trust gating and activation. `PreservedLiveContext` has a dedicated `skills`
field with a byte cap (`apps/rapid/src/host.rs:116`, `MAX_SKILLS_BYTES`).

**The exec path passes `String::new()` for it** (`apps/rapid/src/interactive.rs:1521`).
No skill has ever reached a model. There is also no `skill` tool and no `rapid skills`
command (advertised, absent).

References: Qwen Code ships 12 bundled skills + 22 repo skills, a **skill curator** that
proposes/edits skills from experience, path-conditional auto-activation
(`skill-activation.ts`), and `registerSkillHooks.ts`. Kimi CLI has layered discovery
(Project > User > Extra > Built-in), reads `~/.claude/skills` and `~/.codex/skills` for
cross-vendor compatibility, supports flat `.md` skills, and adds executable **flow skills**.

**Fix.** One line to populate the field, plus a `skill` tool, plus `rapid skills list`.
This is the highest ratio of user-visible capability to work in the repository.

### 4.5 Memory & context engineering

RapidLM has real machinery here — and it is the area where it is *closest* to competitive:
`crates/context-engine` is 30,677 lines with `lsp/`, `compact.rs`, packet compilation,
freshness/lineage; `apps/rapid/src/context_retrieval.rs` does incremental indexing under
`.rapidlm/index/`; `rules_loader.rs` implements a proper AGENTS.md hierarchy with
`.claude/rules` and `.cursor/rules` compatibility (a genuinely good detail none of the
references match exactly).

Gaps:

- **8k budget cap** (§3.4) neutralizes most of it.
- **Memory is read-only.** `load_memory_index` reads `.rapidlm/MEMORY.md`; nothing writes
  it. Qwen Code has an entire `packages/core/src/memory/` subsystem — extraction agents,
  relevance selection, recall eval harnesses, secret scanning, team memory sync, `/forget`,
  `/dream` (offline consolidation). Auggie has `remember` + a server-side context engine.
  Kimi persists via `AGENTS.md` + `/init`.
- **No `/compact` equivalent that a user can invoke**, and no `PreCompact`/`PostCompact`
  hooks (Qwen and Kimi both have them).
- **`repo_search` is materially weaker than any reference's search.** It is
  `line.contains(&args.pattern)` over a hand-rolled tree walk
  (`execute_repo_search`, `apps/rapid/src/exec_tools.rs`): no regex, no ripgrep, no
  case-insensitivity, no context lines, no file-type filter — and it **returns at most one
  hit per file** (`return;` after the first push, commented "one hit per file keeps the
  result compact"). Searching for a symbol used 40 times in one file yields one line.
  Qwen Code has ripgrep + regex + glob + LSP + read-tracking; Auggie has a semantic index
  (`codebase-retrieval`, gated on `indexingEnabled`, with `grep-search` fallback); Kimi
  wraps `ripgrepy`. RapidLM's own `context-engine` supports far more than the wired tool
  exposes, and `apps/rapid/Cargo.toml` declares a `regex` dependency documented as
  "repo.search regex mode + repo.glob pattern matching (Claude parity)" that **no source
  file in `apps/rapid` uses**.
- **No `@file` reference syntax** in the composer; no drag-and-drop/paste of images.

### 4.6 Permissions, approvals, sandbox

RapidLM's `apps/rapid/src/permissions.rs` is good: the exact six-mode lattice
(`default|plan|acceptEdits|auto|dontAsk|bypassPermissions`), `Tool(glob)` allow/ask/deny
with deny-wins, persisted grants, permissiveness ranking for managed-policy ceilings.
**This is at parity with Qwen Code's `PermissionManager` and ahead of Kimi's simpler
yolo/afk model.**

Gaps:

- **No LLM classifier for `auto` mode.** Qwen Code's `permissions/autoMode.ts` +
  `classifier.ts` is a three-layer filter ending in a two-stage LLM classifier
  (stage 1 ~300 ms block-only; stage 2 reviews blocks to cut false positives; fail-closed).
  Without it, RapidLM's `auto` mode is just "allow more things", not "judge each action".
- **No `ask` path in headless.** An `Ask` decision renders as a denial (documented at
  `permissions.rs:8`). Qwen Code's `ask_user_question` and Kimi's `AskUserQuestion` +
  `ApprovalRuntime` let a headless/background agent surface a real question. Kimi's
  `ApprovalRuntime` additionally distinguishes `foreground_turn` from `background_agent`
  approval sources and supports `approve_for_session`.
- **No webhook / script permission policies.** Auggie supports
  `{type: "webhook-policy", webhookUrl}` and `{type: "script-policy", script}` per tool,
  plus `shellInputRegex` scoping and `tool-response`-time policies — a materially richer
  enterprise model than any of the others.
- **Sandbox: macOS Seatbelt only in practice.** `apps/rapid/src/sandbox_exec.rs` is
  explicitly scoped to `HostRestrictedBackend` on non-macOS ("not yet the stronger
  `Container`/`Gvisor` tiers"), and `crates/sandbox`'s tiers are unreachable. `rapid
  doctor` reports `sandbox_availability Unavailable` everywhere (§3.6). Qwen Code ships
  sandbox + Docker; Kimi relies on approval + hooks.
- **No `dangerousRules`/destructive-command detection.** Qwen has
  `permissions/dangerousRules.ts`, `destructive-commands.ts` and `shell-semantics.ts`
  (parses shell operations *across* a compound command). RapidLM matches on tool name and
  argument globs only.

### 4.7 Hooks

RapidLM: **6 events**, command-only, **no matcher** —
`pre_tool_use`, `post_tool_use`, `session_start`, `session_end`, `subagent_start`,
`subagent_stop` (`apps/rapid/src/hooks.rs:24-39`). Every configured hook runs on every
matching stage; there is no way to say "only on writes to `*.py`".

Kimi CLI: **13 events**, `matcher` regex, `timeout`, fail-open semantics.
Qwen Code: **22 events**, four hook types (command, HTTP, prompt, function), `async`,
conditional `if` expressions, `statusMessage`, `env` interpolation, trusted-hook gating,
SSRF guard on HTTP hooks, per-event typed input/output classes, and a `HookPhase`
split (validation vs post-write) for atomic todo updates.

Missing events that matter most: `UserPromptSubmit`, `PreCompact`/`PostCompact`,
`Notification`, `PermissionRequest`/`PermissionDenied`, `Stop`/`StopFailure`,
`PostToolUseFailure`, `InstructionsLoaded`.

### 4.8 MCP

RapidLM wires **stdio only**, ≤8 servers, `{command, args}` schema
(`parse_mcp_servers`, `exec_tools.rs:3353-3384`) — no `env`, no headers, no HTTP/SSE, no
OAuth, no resources, no prompts, no reconnection, no `rapid mcp` command (advertised,
absent). Meanwhile `crates/mcp` exports `StreamableHttpTransport`, `HttpAuthScope`,
`McpTrustStore`, `McpCatalogCache`, `ServerCatalog`, publish limits — all unwired.

All three references support stdio + streamable HTTP, OAuth authorization, `mcp add/list/
remove/auth`, and status display. Qwen Code adds a transport **pool**, per-workspace
budgets, discovery timeouts, retry, resource reading (`read_mcp_resource`), an
MCP classifier, and a marketplace/registry.

### 4.9 ACP / IDE

RapidLM has `crates/acp` (3,980 lines, v1 + v2, `session/new|prompt|load|cancel|
set_mode|request_permission|update`) used **only as a client** to drive external agents
(`apps/rapid/src/external_agents.rs`). There is no `rapid acp` server. Consequence: **zero
IDE integration**, while all three references get Zed/JetBrains/VS Code for free through
ACP, and Qwen Code and Auggie also ship native VS Code extensions.

This is the cheapest large win available: the protocol implementation already exists;
what's missing is a subcommand that binds stdio and maps ACP `session/prompt` onto the same
executor `exec_turn` uses.

### 4.10 Providers, auth, model management

RapidLM: two adapters (`openai_compatible.rs`, `anthropic.rs`), API key from inline TOML or
env var, no interactive login, no model listing, no runtime model switching, and —
critically — `ProviderCapabilities::new(tools=true, streaming=true, vision=false,
caching=false, reasoning=None, …)` **hard-coded for every configured model**
(`apps/rapid/src/model.rs:166-175`).

That means: **no prompt caching** (a 5-10× cost difference on long agent sessions against
Anthropic/OpenAI), **no image input**, **no reasoning-token handling**. There *is* a
`crates/llm-router/src/catalog.rs` with per-model capability descriptors and a
`fallback.rs` chain — the user-config path bypasses it.

References: Qwen Code supports OpenAI/Anthropic/Gemini/Qwen/OAI-compatible/Ollama/vLLM with
`/model` runtime switching and OAuth device flow; Kimi does browser OAuth to Kimi Code, OS
keyring storage, `/model` with live model-list refresh, `/usage` quota display; Auggie does
`auggie login` + `auggie model`.

RapidLM does have a genuine advantage here — a typed router with filter/score/fallback and
a credential-handle boundary (`SecretRef` never materialized until the executor). It is
just not connected to a usable auth story.

### 4.11 Sessions, resume, fork, rewind

RapidLM's `crates/event-ledger` (9,449 lines, append-only, checkpoints, retention,
corruption quarantine) is architecturally the strongest of the four. But:

- `rapid resume`, `rapid fork`, `rewind`, `export` are **advertised and absent** (§3.3).
- `rapid exec` does not resume or continue a session at all — no `--resume`, `--continue`,
  or `--session`.
- `rapid sessions list|search` works but only lists; you cannot re-enter one.
- Fork/rewind exist in the kernel and TUI reducer, and are unreachable because the TUI
  can't run turns.

References: Kimi has `--resume/-r`, `--continue/-C`, `/sessions`, `/fork`, `/undo`
(fork-before-turn + prefill), `/export`, `/import`, `/title`, cross-directory session
browsing. Qwen Code has `/resume`, `/fork`, `/rewind`, `/restore` (checkpointing),
`/branch`, `/history`, `/export`. Auggie has `--continue`, `--session-name`, `session`/
`sessions` subcommands, `/share`.

### 4.12 Headless / automation surfaces

RapidLM `rapid exec` flags: `--verbose`, `--max-wall-time`, `--json-schema`, `--jsonl`.
The `--json-schema` structured-output path (model must call a synthetic tool, validated by
`jsonschema`) is genuinely good and **ahead of Kimi**.

Gaps: no `--output-format json|stream-json`, no `--input-format` for piped turns, no
`--max-turns`, no `--model` flag (env var only), no `--add-dir`, no `--allowed-tools`/
`--disallowed-tools`, no `--mcp-config`, no `--continue`/`--resume`, no
`--final-message-only`/`--quiet`. Kimi has all of these; Qwen Code has these plus
`qwen serve` and IM channels; Auggie has `--print`, `--compact`, `--instruction-file`,
`--workspace-root`, `--dont-save-session`, `--continue-on-error`, `--max-turns`, `--image`.

There is also no CI-shaped entry point (no GitHub Action, no `--github-api-token` style
integration; Qwen Code has `/setup-github`, Auggie has extensive GitHub/Linear/Jira/
Confluence/Notion/Supabase/Glean integrations).

### 4.13 Multimodal

`ContentPart::{Text, Image, ImageData}` exists (`crates/llm-router/src/provider.rs:369`)
and `exec_tools.rs` can render PNG/PDF reads — but `vision: false` on every configured
model (§4.10) and there is no image-bearing tool (`zoom_image`, `display_image`,
`ReadMediaFile`) and no way to attach an image to a prompt. Net: **no multimodal in
practice**. All three references support image input; Qwen Code additionally has
`image_gen`, voice input (`audio-capture`, `qwen-live`, `/voice`), and artifact publishing.

### 4.14 Computer use

`crates/computer-use` (20,067 lines: browser/CDP/DOM/AX, desktop) and `crates/mobile-sim`
(14,794 lines) are the largest single investment in the repository after the context
engine. **Neither is reachable.** `mobile-sim` has *zero* references outside its own crate;
`computer-use` is referenced only by `apps/rapid/src/computer_runtime.rs` (214 lines),
which is itself referenced only by `lib.rs`'s `pub mod` declaration. `browser.act` and
`mobile.act` exist in the orphaned `GatewayTool` catalog. `rapid computer` is advertised
and absent.

Qwen Code ships this as working product: `cua-driver`, `mobile-mcp`, a Chrome extension, a
`computer-use` bundled skill, and `packages/core/src/tools/` browser integration. Auggie
has `open-browser`. Kimi relies on MCP (`chrome-devtools-mcp`).

### 4.15 Observability, cost, telemetry

- `crates/telemetry` (3,249 lines): **zero external references**. No OTel export, no
  metrics, no traces leave the process.
- `crates/trajectory` (137 lines) and `crates/insights` (182 lines) are stubs; `rapid
  insights` returns usage errors.
- Cost: `rapid exec` prints `tokens used: N (cost)` — good. But no `/stats`, no session
  cost accumulation, no per-model breakdown, no quota display.
- No trace visualizer. Kimi's `/vis` starts a server rendering wire-event timelines,
  context messages and usage stats; Qwen Code has `/stats`, `/insight`, OTel telemetry
  (`packages/core/src/telemetry/`), and `agent-statistics.ts`.

### 4.16 Distribution & updates

RapidLM has **no distribution story**: no npm package (the root `package.json` is
explicitly "TypeScript SDK and schema tooling only"), no Homebrew formula, no installer
script, no binary releases in `.github/workflows` beyond a `release-matrix.yml`, no
`rapid update` (advertised, absent), no version command.

Qwen Code: curl/irm standalone installers, npm, Homebrew, Docker, desktop app releases.
Kimi: PyPI, standalone PyInstaller binary, Nix flake, `.spec`, `/upgrade`.
Auggie: npm global install.

### 4.17 TUI/UX polish

Missing vs all three: themes (RapidLM has only `ColorMode::{Enabled,Disabled}` honouring
`NO_COLOR`), `@file` completion, image paste, external-editor integration (`Ctrl-O`), vim
mode, statusline, shell mode (Kimi's `Ctrl-X`), fuzzy slash-command completion, `/help`
pager, `/feedback`, `/btw` side questions, task browser, session picker, `/init`, i18n.

RapidLM's 27 slash commands are largely inspectors (`/trace`, `/insights`, `/knowledge`,
`/playbook`, `/handoff`, `/takeover`) projecting state that nothing produces, because no
turn ever runs.

### 4.18 Docs

Kimi ships a full bilingual docs site (`docs/en`, `docs/zh`) with per-command reference
pages. Qwen Code ships `docs/` + a `docs-site/` in 7 languages. RapidLM has 33 items under
`docs/` aimed at implementers, plus a 76 KB `gaps.md` and a 267 KB `newtask.md` — extensive
internal design documentation, **no user documentation**.

---

## 5. Built but unreachable — the dead-weight inventory

| Crate / module | Lines | Reachable from a shipped command? |
|---|---:|---|
| `crates/computer-use` | 20,067 | ❌ only via `computer_runtime.rs`, itself unused |
| `crates/tool-gateway` | 5,270 | ❌ **not referenced by any `Cargo.toml` in the workspace** |
| `crates/mobile-sim` | 14,794 | ❌ zero external references |
| `crates/plugin-host` (incl. skills 2,307) | 12,860 | ⚠️ only `rapid plugins`; skills never reach the model |
| `crates/telemetry` | 3,249 | ❌ |
| `crates/acp` | 3,980 | ⚠️ client-only, no server |
| `crates/vcs` | 1,703 | ❌ |
| `crates/harness` | 1,032 | ❌ |
| `crates/handoff` | 702 | ❌ |
| `crates/knowledge` | 266 | ❌ |
| `crates/insights` | 182 | ⚠️ `rapid insights` returns usage error |
| `crates/trajectory` | 137 | ❌ |
| `apps/rapid/src/host_runtime.rs` | 398 | ❌ declared, never used |
| `apps/rapid/src/preview.rs` | 216 | ❌ declared, never used |
| `apps/rapid/src/computer_runtime.rs` | 214 | ❌ declared, never used |
| `GatewayTool` catalog (inside `tool-gateway`) | 12 tools | ❌ parallel authority to `exec_tools.rs` |

**~47,400 lines of crates with no external caller** (`computer-use`, `mobile-sim`,
`tool-gateway`, `telemetry`, `vcs`, `harness`, `handoff`, `knowledge`, `insights`,
`trajectory`), plus ~830 lines of orphaned binary modules, plus `crates/acp` reachable only
as a client and `crates/plugin-host` reachable only from one CLI command. `crates/tool-
gateway` is the starkest case: it is listed in the workspace `members` array but appears in
**no other `Cargo.toml`**, so nothing links it. Every one of these crates is *tested* — they
contribute to the 3,135-test figure while contributing nothing to the product.

This is not an argument to delete them. It is the argument that RapidLM's problem is
**delivery, not construction**: the code that would beat the references on several axes
exists and is not plugged in.

---

## 6. Where RapidLM genuinely leads

Stated conservatively, and checked against all three references:

1. **Durable event ledger with checkpoints, retention and corruption quarantine**
   (`crates/event-ledger`, 9,449 lines). Kimi stores JSONL session logs; Qwen Code stores
   session files + checkpoints; neither has an append-only ledger with the same recovery
   guarantees. **Caveat:** Qwen Code's `workflow-journal.ts` and goal checkpointing narrow
   this considerably.
2. **Capability broker with typed leases** (`crates/capability-broker`, 14,171 lines,
   50 dependent files — the most-used crate in the workspace). No reference has an
   equivalent; Auggie's webhook/script policies are the nearest analogue and are
   coarser.
3. **Credential handles that stay unmaterialized until the executor boundary**
   (`SecretRef`/`SecretValue`, `crates/auth`). Genuinely better than any reference's
   handling.
4. **`--json-schema` structured output with a hard guarantee** — the model must call a
   synthetic tool whose arguments validate, or the run fails. Qwen Code has
   `structured_output`; Kimi has none. RapidLM's fail-closed framing is stronger.
5. **AGENTS.md hierarchy with `.claude/rules` + `.cursor/rules` compatibility**
   (`rules_loader.rs`) — cleanest cross-vendor instruction loading of the four.
6. **Engineering discipline:** `#![forbid(unsafe_code)]`, pinned toolchains, no-secrets CI
   on pull requests, schema goldens that never auto-rewrite, 3,135 tests, clean
   `cargo check --workspace --all-targets`.
7. **Single static binary, no Node/Python runtime.** Real operational advantage — currently
   unrealized because there are no releases.

Note what is *not* on this list: evidence-gated goal completion. Qwen Code has
`packages/core/src/goals/` with `goal-evidence.ts`, `goal-verifier.ts`, `goalJudge.ts`,
`goal-checkpoint-verifier.ts` and three model-callable goal tools. RapidLM's goal system is
deeper in its durability guarantees but shallower in reach — the model cannot touch it.

---

## 7. Prioritized remediation roadmap

**Gate 0 — make the product exist (blocking; nothing else matters until these ship)**

| # | Change | Files |
|---|---|---|
| 0.1 | Add `prompt` to `SubmitTurn`; route the kernel turn service through the same executor `exec_turn` builds | `crates/kernel/src/client.rs`, `crates/kernel/src/turn/`, `apps/rapid/src/interactive.rs:1560` |
| 0.2 | First-run trust prompt + `rapid trust grant\|revoke\|status` | `apps/rapid/src/interactive.rs`, new p9 command |
| 0.3 | Thread model `context_window`/`max_tokens` into `build_live_context` | `apps/rapid/src/interactive.rs:1569`, `model.rs:161` |
| 0.4 | Cut `CLI_USAGE` to implemented commands; move the rest to a roadmap doc | `apps/rapid/src/interactive.rs:193` |
| 0.5 | Populate `PreservedLiveContext::skills` from `plugin_host::skills` | `apps/rapid/src/interactive.rs:1521` |
| 0.6 | Make `rapid doctor` gather real inputs | `apps/rapid/src/p9_commands.rs` |

**Gate 1 — reach parity on the surfaces that create distribution**

| # | Change | Why |
|---|---|---|
| 1.1 | `rapid acp` stdio server over the existing `crates/acp` | Buys Zed + JetBrains + any ACP client at once |
| 1.2 | `rapid daemon` binding the existing `IpcServer` | Makes the 6k-line TS SDK functional |
| 1.3 | `web_search` + `save_memory` + `skill` + `goal_update`/`goal_get` tools | The four highest-value missing tools |
| 1.4 | Per-model capabilities: caching, vision, reasoning from a catalog instead of hard-coded `false` | Prompt caching alone is a large cost/latency win |
| 1.5 | `rapid exec --resume/--continue/--model/--output-format` | Table stakes for automation |
| 1.6 | Hook matchers + `UserPromptSubmit`, `PreCompact`/`PostCompact`, `Stop`, `Notification` | Closes most of the hook gap cheaply |
| 1.7 | Release pipeline: binaries per platform + install script + `rapid update` | No adoption without it |

**Gate 2 — competitive differentiation**

| # | Change |
|---|---|
| 2.1 | MCP: streamable HTTP + OAuth + resources (the `crates/mcp` transports already exist) |
| 2.2 | Project-defined subagents via the existing `agent_defs` loader; parallel subagents |
| 2.3 | LLM classifier for `auto` mode (Qwen's two-stage design is the template) |
| 2.4 | `lsp` tool over the existing `context-engine/src/lsp/` |
| 2.5 | Wire `computer-use` behind `browser.act`, or delete it |
| 2.6 | Retire `GatewayTool` or make it the single authority — not both |
| 2.7 | Real user documentation |

---

## 8. Complete parity checklist

Legend: ✅ present and reachable · ⚠️ present but not reachable / partial · ❌ absent.

| Capability | Qwen | Auggie | Kimi | RapidLM |
|---|:--:|:--:|:--:|:--:|
| Interactive TUI running turns | ✅ | ✅ | ✅ | ❌ |
| Headless one-shot | ✅ | ✅ | ✅ | ✅ |
| Session resume / continue | ✅ | ✅ | ✅ | ❌ |
| Session fork | ✅ | ✅ | ✅ | ⚠️ |
| Rewind / undo turn | ✅ | ❌ | ✅ | ⚠️ |
| Checkpoint / restore files | ✅ | ✅ | ❌ | ⚠️ |
| Export / import session | ✅ | ✅ | ✅ | ⚠️ |
| Plan mode | ✅ | ✅ | ✅ | ✅ |
| Todo list tool | ✅ | ✅ | ✅ | ✅ |
| Read / write / edit / glob / grep | ✅ | ✅ | ✅ | ✅ |
| Shell with background jobs | ✅ | ✅ | ✅ | ✅ |
| Web fetch | ✅ | ✅ | ✅ | ✅ |
| Web search | ✅ | ✅ | ✅ | ❌ |
| Agent-writable memory | ✅ | ✅ | ⚠️ | ❌ |
| Skills reaching the model | ✅ | ✅ | ✅ | ❌ |
| Skill auto-activation | ✅ | ❌ | ⚠️ | ❌ |
| Subagents | ✅ | ✅ | ✅ | ⚠️ |
| File-defined subagents | ✅ | ✅ | ✅ | ❌ |
| Parallel subagents | ✅ | ✅ | ✅ | ❌ |
| Agent teams | ✅ | ✅ | ❌ | ❌ |
| Declarative workflows | ✅ | ❌ | ✅ | ❌ |
| Cron / scheduled runs | ✅ | ✅ | ❌ | ⚠️ CLI-only |
| Structured user questions | ✅ | ✅ | ✅ | ⚠️ free-text |
| LSP integration | ✅ | ❌ | ❌ | ⚠️ |
| Notebook editing | ✅ | ❌ | ❌ | ❌ |
| Image input | ✅ | ✅ | ✅ | ❌ |
| Image generation | ✅ | ❌ | ❌ | ❌ |
| Voice input | ✅ | ❌ | ❌ | ❌ |
| Computer use / browser | ✅ | ⚠️ | ⚠️ MCP | ⚠️ |
| Mobile automation | ✅ | ❌ | ❌ | ⚠️ |
| MCP stdio | ✅ | ✅ | ✅ | ✅ |
| MCP HTTP/SSE | ✅ | ✅ | ✅ | ⚠️ |
| MCP OAuth | ✅ | ✅ | ✅ | ❌ |
| MCP resources | ✅ | ? | ⚠️ | ❌ |
| `mcp` management command | ✅ | ✅ | ✅ | ❌ |
| Hooks | ✅ 22 | ✅ | ✅ 13 | ⚠️ 6, no matcher |
| HTTP hooks | ✅ | ✅ webhook policy | ❌ | ❌ |
| Plugins / extensions | ✅ | ✅ marketplace | ✅ | ⚠️ CLI-only |
| Custom slash commands | ✅ | ✅ | ⚠️ skills | ❌ |
| Output styles / personas | ✅ | ✅ | ✅ agentspec | ❌ |
| Permission modes | ✅ 6 | ✅ rules+policies | ✅ | ✅ 6 |
| LLM permission classifier | ✅ | ❌ | ❌ | ❌ |
| Sandbox | ✅ | ✅ cloud | ❌ | ⚠️ macOS only |
| Git worktree isolation | ✅ | ✅ | ❌ | ⚠️ |
| ACP server | ✅ | ✅ | ✅ | ❌ |
| VS Code extension | ✅ | ✅ | ✅ | ❌ |
| JetBrains / Zed | ✅ | ✅ | ✅ | ❌ |
| Desktop app | ✅ | ✅ | ❌ | ❌ |
| Web UI | ✅ | ❌ | ✅ | ❌ |
| Daemon / multi-client server | ✅ | ✅ | ✅ wire | ❌ |
| SDK (working) | ✅ ×3 | — | ✅ | ❌ |
| IM / chat integrations | ✅ ×4 | ✅ Slack | ❌ | ❌ |
| Issue-tracker integrations | ⚠️ | ✅ ×6 | ❌ | ❌ |
| Multiple providers | ✅ | ❌ | ✅ | ⚠️ 2 |
| Interactive login / OAuth | ✅ | ✅ | ✅ | ❌ |
| Runtime model switching | ✅ | ✅ | ✅ | ❌ |
| Prompt caching | ✅ | ✅ | ✅ | ❌ |
| Cost / usage reporting | ✅ | ✅ | ✅ | ⚠️ per-run only |
| OTel telemetry | ✅ | ✅ | ✅ | ⚠️ crate unused |
| Trace visualizer | ⚠️ | ✅ | ✅ | ❌ |
| Themes | ✅ | ✅ | ✅ | ❌ |
| Vim mode | ✅ | ✅ | ❌ | ❌ |
| External editor | ✅ | ✅ | ✅ | ❌ |
| `@file` references | ✅ | ✅ | ✅ | ❌ |
| Shell mode in TUI | ❌ | ✅ | ✅ | ❌ |
| `/init` project bootstrap | ✅ | ✅ | ✅ | ❌ |
| Self-update | ✅ | ✅ | ✅ | ❌ |
| Binary / package distribution | ✅ | ✅ | ✅ | ❌ |
| User documentation | ✅ | ✅ | ✅ | ❌ |
| i18n | ✅ 7 | ❌ | ✅ 2 | ❌ |
| Structured-output schema enforcement | ✅ | ⚠️ | ❌ | ✅ |
| Durable event ledger | ⚠️ | ? | ⚠️ | ✅ |
| Capability leases | ❌ | ⚠️ | ❌ | ✅ |
| Unmaterialized credential handles | ❌ | ? | ❌ | ✅ |
| Evidence-gated goals | ✅ | ❌ | ❌ | ⚠️ CLI-only |
| Memory-safe implementation | ❌ | ❌ | ❌ | ✅ |

---

## 9. Method notes and limitations

- **Qwen Code and Kimi CLI claims are source-grounded** against full clones at the commits
  named above, and cite paths.
- **RapidLM claims are source-grounded and additionally executed.** `cargo check
  --workspace --all-targets` and `cargo build --bin rapid` both succeed; every "advertised
  but absent" command in §3.3 was run against `target/debug/rapid` and observed to exit 2;
  `rapid exec "say hi"` was run in a scratch git repository and observed to complete a real
  model turn (133 tokens) while reporting workspace tools disabled.
- **Auggie claims are recovered from a 13 MB minified bundle** — tool names, slash
  commands, CLI subcommands and the permission-rule Zod schemas were extracted with high
  confidence; the exact model-callable tool *count* and per-tool schemas were not fully
  recoverable, and Augment runs much of its agent server-side, so absence of a string in
  the bundle is not evidence of absence in the product. Rows marked `?` reflect that.
- **This document supersedes `gaps.md` on three points** where that document is now stale:
  RapidLM wires 15 tools (not 2), dispatches them in parallel (not sequentially), and sends
  per-call `tool_result` messages with `tool_call_id` (not a flat text report).
- **Not assessed:** benchmark task-success rates, latency, token efficiency, or model
  quality. This is a capability-surface analysis only. A fixed-model eval against SWE-bench
  or an internal suite would be the right next measurement, and is not currently possible
  for RapidLM because the harness cannot enable workspace tools (§3.2).
