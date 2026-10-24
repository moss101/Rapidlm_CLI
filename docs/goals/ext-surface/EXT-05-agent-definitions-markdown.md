# EXT-05 — Agent definitions as Markdown, with tool lists, narrowing rules, a step cap and a selectable main agent

**Status:** draft behavior spec (not yet scheduled; see [README](README.md))  
**Size:** M–L (five slices; slices 1–3 are independent of 4–5)  
**Depends on:** SEAM-04 as delivered. Slice 5 depends on [EXT-01](EXT-01-mcp-tool-surface-control.md) slice 1. Shares a frontmatter parser with [EXT-04](EXT-04-skills.md).  
**Invariants touched:** core 1, 3, 12, 13; SEAMS S1, S3, S5, S7, S9, S11  
**Baseline:** `05ad9d6`. Every "Today" line is a reading of that tree and must be rechecked at Phase 0 (SEAMS §5.1); lines marked *(spot-checked)* were re-read by the author of this spec. `AD` = `crates/agent-runtime/src/agent_defs.rs`, `ET` = `apps/rapid/src/exec_tools.rs`, `IA` = `apps/rapid/src/interactive.rs`.

---

## 1. Intent

A project should be able to describe a specialist — "a read-only reviewer that may use these tools and nothing else, that cannot spawn further agents, and that stops after twelve steps" — in a short Markdown file next to the code, and to say which agents the main model may delegate to. Today the same idea needs a TOML file in a coarse vocabulary, has no step cap, cannot name individual tools, and the main agent cannot be chosen at all. This spec:

1. adds a **Markdown form** of the existing definition (frontmatter + instructions body) that loads into the same in-memory type under the same validation;
2. adds **fine-grained narrowing**: tool allow/deny patterns, `ask`/`deny` permission rules, a model-step cap;
3. adds a **spawn allowlist** and `when_to_use` text so delegation is steered by the definitions;
4. lets a user **select a primary agent** for a session;
5. (conditional) lets a definition opt a subagent into specific MCP tools.

Everything narrows. A definition is a request; the permission lattice and the managed policy decide (SEAMS S5, core invariant 13).

(Class names are the ten in `RoleToolClass`: `read`, `write`, `exec`, `net`, `browser`, `mobile`, `mcp`, `plugin`, `git`, `secret`. Tool names are the wire names such as `repo_read`, `repo_search`, `repo_glob`, `workspace_read`, `workspace_patch`, `shell_exec`; `rapid tools` prints the full list.)

## 2. Today

- **Definitions are TOML only.** `schema = "rapidlm.agent_defs.v1"` with one closed `[agent]` table (`AD:373-545, 52-66`). Fields: `id`, `description` (≤ 512 bytes), `base_role` (or the older `role`), `tools`, `instructions` (≤ 8 KiB), `model`, `reasoning_effort`, `inputs`, `outputs`. **`tools` is a list of ten coarse tool *classes*** (`crates/agent-runtime/src/role_profile.rs:16-27`), not tool names, and an omitted `tools` means an *empty* surface, not "inherit the role" (`AD:473`).
- **Where they load from, in what order.** Built-in > project (`<project>/.rapidlm/agents`, trusted projects only) > user (`<home>/agents`; the constant `USER_DEFS_DIR` at `AD:40` is unused) — `AD:807-850`, `agent_types.rs:34-47`. A duplicate id returns `DuplicateId` for either origin and `layered_inventory` turns that into a rejection of the **whole** directory (`AD:757-770, 841-844`); only a collision with a built-in id rejects a single file (`AD:772-781`). Only `*.toml`, non-recursive, ≤ 128 files. File symlinks are refused but **a symlinked `agents/` directory is followed** (`AD:682-720`).
- **Validation.** `validate_grants` (`AD:549`) checks the requested classes against the base role's surface (`role_profile.rs:107-250`), so a definition can only narrow. `RoleRegistry` is consulted by `agent_defs.rs:194, 550` (`validate_grants`), `agent_executor.rs:643` and `specialist.rs:106`, but not by `apps/rapid`.
- **CLI.** `rapid agents list|validate|scaffold [--dir]` (`p9_commands.rs:2790-2890`, registered `IA:853`). `list` and `validate` read the project directory **without a trust check**. `inspect|cancel` in `cli-command-reference.md:37` is roadmap.
- **The spawn path reads the definitions.** SEAM-04-1…5 are complete (`docs/goals/seams-implementation-tasks.json`): inbox edges, admission, declared-outputs check, and the `/agents` types panel. `task_spawn` takes `prompt` (≤ 16 KiB), `type` (enum of loaded ids, default `general-purpose`), `description` (accepted and dropped, `ET:7998`), `write_scope` and `background` (`ET:9763-9807, 11109-11136`); the type resolves at `ET:6709`. A child gets the definition's model and effort (floored by managed policy, `IA:3496-3532`), a read-only tool set unless the surface has write or exec — then its own worktree (`IA:3708, 3870`), `PermissionMode::for_subagent()` (`permissions.rs:872`) with the `write_scope` (`IA:3902-3905`), and a prompt of live context plus "Declared outputs" and "Agent type" blocks, ≤ 32 KiB (`IA:3994-4030`).
- **Depth is 1 and hard-coded.** Children always have nested spawning disabled *(spot-checked, `ET:3534`, `IA:3921`)*. `DELEGATION_MAX_DEPTH = 3` (`delegation.rs:15`) is never called from `apps/rapid`, although its doc calls it the single boundary.
- **Tool narrowing is class-based.** `narrow_to_role_surface` (`ET:3546`) intersects classes and is enforced twice: the tool is hidden (`ET:10629`) and the call is denied (`ET:4251`). `role_surface_allows` (`ET:7326`) maps each tool to one class; **every `mcp__*` tool is `Class::Mcp`** (`ET:7337`), unknown tools are denied. Children never register or share MCP (`LiveSubagentRunner`, `IA:3399-4420`, does not mention it). Permission rule identifiers are exact tool names; `*` is illegal there (`permissions.rs:122-171, 270-283`).
- **No step cap.** `AgentBudget` has no steps field (`agent/model.rs:161`); `budget_from` hard-codes `MAX_MODEL_STEPS = 32` *(spot-checked, `turn.rs:18`)* and `TurnBudget::new` rejects more (`turn.rs:1083`); the child spec sets no budget (`IA:4031`).
- **No main-agent selection.** `parse_exec_args` accepts no agent flag (`IA:2295-2360`); every top-level turn hard-codes `AgentRole::Coder` (`IA:5437, 14158, 14753`, and a fourth site in `run_workflow_agent_step`, `IA:15414`). Shift-Tab toggles `PermissionMode::Plan`, a permission mode, not a role (`IA:7506, 10883`; `ui_config.rs:101`).
- **No frontmatter parser for agents; no YAML crate** in `Cargo.lock`. The two existing parsers — the skills loader's closed subset (`SK:1333`) and the custom-commands `key: value` reader (`CC:308`) — are private and cover different subsets.
- **Repository files.** `agents/*.md` and `system-prompts/*.md` at the repo root have no Rust references (they are "execution hint only" documents with dotted tool names such as `repo.search`, while runtime names use underscores — `ET:33-122`); the real main prompt is compiled in `crates/agent-runtime/src/prompt_stack.rs:173`.

## 3. Behavior

### 3.1 One definition, two syntaxes (slice 1)

A definition file is `<dir>/<id>.md` or `<dir>/<id>.toml` in the same directories, under the same trust rules and the same precedence as today (built-in > project > user, plus `[agents] extra_dirs` below user — at most 8 directories). Both syntaxes produce the same in-memory definition, run through **one** validator.

**Markdown form.** Frontmatter delimited by `---` lines, then the body.

```markdown
---
name: reviewer
description: Read-only code reviewer
when_to_use: After a change is complete and before it is committed
base_role: reviewer
mode: subagent
model: review
reasoning_effort: high
tool_classes: read git
allow_tools: repo_* workspace_read
deny_tools: repo_glob
permission:
  "workspace_patch(docs/*)": ask
max_steps: 12
inputs: diff
outputs: findings
---
You review changes for correctness and security. Report findings only; …
```

- **Parser vs schema.** The shared module only tokenizes the closed subset; each consumer validates its own closed key set. The skills loader's privilege-key denylist (`permission`, `tools`, `hooks`, …, `SK:99-135`) is part of the **skills schema**, not of the parser, and stays with it; the agent-definition schema accepts `permission`, `allow_tools` and `deny_tools` because every one of them can only narrow.
- **Frontmatter** is the closed subset the skills loader already parses (scalars, block lists, whitespace-separated scalars for short lists), moved to a shared module in `crates/protocol` (both `plugin-host` and `agent-runtime` depend on it and not on each other) and extended with exactly one construct it lacks today: a one-level nested map whose keys may be double-quoted (needed for `permission`, whose keys contain parentheses, spaces and `*`) (decision D-1). `name` defaults to the file stem and, if given, must equal it. Unknown keys are rejected (not ignored), exactly as the TOML table is closed today.
- **Body** becomes `instructions` (≤ 8 KiB, the existing cap; longer is rejected). It is **appended** to the base role's prompt in the labelled overlay section the TOML `instructions` field uses today. There is no templating and no way to replace the base prompt (S5).
- **Equivalence.** `tools` in the TOML form and `tool_classes` in the Markdown form name the same ten classes; the TOML schema is bumped to `rapidlm.agent_defs.v2` (v1 files keep parsing) and gains the same new keys under the same names, so every definition can be written in either syntax. If `<id>.md` and `<id>.toml` both exist in one directory, **both are rejected** with a message naming each file.
- **Hardening.** A symlinked `agents/` directory is skipped with a reason, like the file symlinks (decision D-4).
- **CLI.** `rapid agents list` shows id, format, source directory, `mode`, the effective tool surface (classes and patterns), caps, and rejected files with reasons — and says "not loaded: untrusted project" instead of reading an untrusted directory. `validate [--dir]` checks every new key and warns about patterns that can never match (naming the closest real tool names from `rapid tools`). `scaffold --format md|toml`.

*Implementation notes (EXT-05-1).* (a) The front matter becomes the same `[agent]` table the TOML form has, read by the one validator, so equivalence holds by construction and a property test (30 000 generated definitions, refused ones included) checks it; the Markdown form has no `instructions` key, since the body is the instructions, and no `tools` (it is `tool_classes`). (b) The body is trimmed and its CRLF read as LF; an empty body is no instructions. (c) The tokenizer is `protocol::frontmatter`; the skills loader reads through it, and the old reader is kept in the skills tests as an oracle that a generated corpus is compared against. The nested map (`permission`) is accepted by the tokenizer under a rule the agent schema will turn on in slice 2; slice 1 does not. (d) `rapid agents list|validate` resolve trust as the spawn path does; `--dir <path>` names a directory to read whatever its project's trust (decision: the files are inert and checking one before trusting the project is what `validate` is for), and `validate` in an untrusted project validates nothing and exits 3. (e) Behavior changes to call out: a symlinked `agents/` directory is skipped with a reason (D-4); a definition file that is not valid UTF-8 is refused on its own instead of failing the whole directory; any `*.md` file in a definitions directory is read as a definition, so a README there is reported as refused; `list` and `validate` no longer create the RapidLM home. (f) `[agents] extra_dirs` is read only from the user's config and follows `[skills] extra_dirs` (same reader, same bounds). `USER_DEFS_DIR` is now `agents`, relative to the RapidLM home, which is where the loader always read. *Follow-up (EXT-05-1-R):* a `.md` and a `.toml` of one name clash whatever sorts between them; a trusted project's directory is read only if it resolves inside the project; the opening fence is a line of its own; names in a one-line list split on ASCII white space; what a file says is shown clean and bounded in `rapid agents`, `/agents` and the errors; `rapid agents --help` prints the command's own usage; `validate` prints a summary and refuses a named directory that is not there; the skills differential test now also compares cancellation, an expired time and values past the scalar bound, and the tokenizer judges an item with no key open before the item itself, as the old reader did.

### 3.2 Tool lists, permission rules and the step cap (slice 2)

New keys (both syntaxes):

- `allow_tools` — patterns over **tool wire names**, with exactly the matcher and bounds of EXT-01 §3.1 (`*` any run, `?` one character; ≤ 64 patterns, each ≤ 128 bytes). Absent means "no extra restriction beyond the classes".
- `deny_tools` — patterns subtracted last.
- `permission` — a map from an existing permission-rule pattern (`Tool` or `Tool(arg-glob)`, the grammar of project `permissions` rules) to **`deny` or `ask` only**. `allow` is not accepted: an agent file can restrict, never pre-approve (S5).
- `max_steps` — an integer 1–32 (the existing hard ceiling).

**Effective tool surface** = `(base role ∩ tool_classes) ∩ allow_tools (if present) − deny_tools`. It is enforced at the two existing points: the tool is hidden from the child's tool list, and a call to a tool outside the surface is denied with the typed `tool_not_allowed_for_agent` (`<tool>` is not available to agent `<id>`). `permission` rules are applied to the child's permission context after `write_scope` (`IA:3902`) using the existing rule mechanism (`with_rules`, `with_denied_tools`), with the managed policy still outranking everything.

**Step cap.** `AgentBudget` gains a steps field, plumbed through `budget_from` into the run, clamped to 32. When the cap is reached the child ends with the **existing** stop reason `budget_exhausted` and the detail `steps`, and returns what it has as its report, exactly as a spent token budget does. No new stop-reason token is introduced (decision D-5).

**Validation (load time).** An `allow_tools` or `deny_tools` pattern that matches no known tool, a `permission` key that does not parse as a rule, a value other than `deny`/`ask`, and a `max_steps` outside 1–32 are rejected (patterns that match nothing only warn in `validate`, because an MCP tool may not be connected yet).

### 3.3 Delegation steering (slice 3)

- `mode` — `subagent` (default; offered to `task_spawn`), `primary` (selectable as a main agent only, 3.4) or `both`.
- `when_to_use` (≤ 256 bytes) is appended to the definition's description in the `task_spawn` type descriptions. The agent list offered to the model keeps the **existing** bound (`MAX_TYPES_DESCRIPTION_BYTES`, 2 048 bytes, `ET:7113`); overflow is summarised, so a configuration without the new keys produces the same description.
- `spawn` — an allowlist of agent ids this agent may delegate to. For a **primary** agent it filters the `type` values offered to `task_spawn` and is checked at the spawn site; a spawn of a type outside it returns `agent_type_not_allowed` listing the allowed ids. For a subagent-only definition a non-empty `spawn` is **rejected** at load ("nested spawning is disabled"), because children cannot spawn (decision D-3). Continuing an existing child (SEAM-04) is not a spawn and is exempt.
- `description` in `task_spawn`, accepted and dropped today, is shown in the child's panel row and recorded on the spawn event (a presentation fix, no behavior change).

### 3.4 Selecting the main agent (slice 4)

A definition with `mode: primary` or `both` can be the session's main agent:

- `rapid exec --agent <id>`, `rapid --agent <id>` (terminal UI launch) and `[agents] default_primary = "<id>"` in user `config.toml` (flag > config).
- It applies as an **overlay on the main role** with the same narrowing as for children: instructions appended in the labelled section, the tool surface narrowed, `model`/`reasoning_effort` used when managed policy allows, `max_steps` capping each run, `permission` rules applied. It can never widen the main role.
- **Fixed for the session.** The selection is recorded as `agent.selected { id, definition_digest }` when the session starts. Switching mid-session is not supported (a new session or fork is the way); this keeps the compiled prompt stable. On resume the recorded id is re-applied from the *current* file; if its digest differs from the recorded one the resume prints a one-line warning and uses the current file (decision D-6).
- Plan mode (the permission mode) is independent and still wins: a primary agent in plan mode cannot write.
- Unknown id, a `subagent`-mode id, an untrusted project's definition and a managed-policy refusal are each a typed error before any model call, with a hint naming `rapid agents list`.

### 3.5 MCP tools for subagents (slice 5, conditional)

Today a child has no MCP tools. If decision D-2 is "yes", a child receives MCP tools **only** when its definition's `allow_tools` names them (`mcp__docs__*`): the parent's registry is shared read-only, the tools pass through the server's EXT-01 filters first, each call is classified and asked exactly as for the parent, and deferred servers (EXT-01) are not loaded for children — a child using `allow_tools` receives the matching tools as ordinary non-deferred entries within the 20 KiB / provider bounds, or the definition is rejected at spawn with `too_large`. A definition that does not name MCP tools gets none, as today.

## 4. Configuration surface

```toml
# ~/.rapidlm/config.toml
[agents]
extra_dirs      = ["~/shared-agents"]
default_primary = "docs-writer"
```

```toml
# <project>/.rapidlm/agents/tester.toml  (schema v2; the Markdown form is equivalent)
schema = "rapidlm.agent_defs.v2"
[agent]
id           = "tester"
description  = "Runs and triages tests"
base_role    = "tester"
tools        = ["read", "exec"]
allow_tools  = ["shell_exec", "repo_read", "repo_search"]
deny_tools   = ["workspace_patch"]
max_steps    = 16
mode         = "subagent"
when_to_use  = "After code changes, to run and summarise the test suite"
[agent.permission]
"shell_exec(rm *)" = "deny"
```

## 5. Records and contracts

| Surface | Change |
|---|---|
| Ledger `agent.selected` | `{ id, definition_digest, mode }` when a primary agent is selected; registered like EXT-01 §5 |
| Spawn record | gains `definition_digest` (additive) and `description` |
| Typed failures | `tool_not_allowed_for_agent`, `agent_type_not_allowed` |
| Schema | `rapidlm.agent_defs.v2` fixture under `crates/protocol` (none exists for v1 — `AD:27` only) |
| SDK catalog | `agent_spec` in `wire.v1.json` is checked for new fields; regenerate with `pnpm generate` and `pnpm generate:check` |
| Panels | `/agents` types rows show mode, caps and the effective surface summary |

## 6. Failure and edge behavior

- A definition that names a model profile that is not configured, or that managed policy refuses, is rejected at load with the existing model-resolution error and remediation.
- Duplicate ids across directories follow the existing precedence; the shadowed definition is reported once by `rapid agents list`.
- A child's `max_steps` reached while a tool call is pending completes the current step's calls, then ends; no call is abandoned mid-flight.
- Body or frontmatter containing NUL or invalid UTF-8 is rejected.
- A definition file edited during a session takes effect at the next spawn (definitions are loaded per spawn, as today); a selected primary agent is not reloaded mid-session.
- Windows: path handling and symlink checks use portable fixtures or typed unavailability.

## 7. Acceptance criteria

- **AC-01** All existing agent-definition tests pass unchanged; a v1 TOML file loads and behaves identically; with no new keys the spawn path, the `task_spawn` description and the `/agents` panel are unchanged.
- **AC-02** A Markdown definition and the equivalent TOML definition produce **identical** in-memory definitions (property test over a corpus); the Markdown body appears in the child's prompt in the overlay section and never replaces the base prompt (an attempt to template over it is rejected).
- **AC-03** Rejections have typed reasons shown by `list`/`validate`: unknown key, `name` ≠ file stem, `.md`/`.toml` collision (both rejected), body over 8 KiB, bad pattern, `allow` in `permission`, `max_steps` out of range, non-empty `spawn` on a subagent-only definition, symlinked directory.
- **AC-04** `rapid agents list|validate` on an untrusted project loads nothing and says why.
- **AC-05** Effective-surface arithmetic: fixtures for classes ∩ allow − deny, an `allow_tools` that can only narrow, and a definition that tries to name a tool outside its base role (rejected or no effect, never widening). The tool is hidden *and* a forced call returns `tool_not_allowed_for_agent` (a stub model that calls it).
- **AC-06** `permission` rules deny/ask as stated for the child's calls and for no one else; managed policy still outranks them (gate test).
- **AC-07** `max_steps = N` ends a stub child after exactly N model steps with `budget_exhausted` / `steps` and a usable report; N > 32 is rejected at load.
- **AC-08** `spawn` on a primary agent filters the offered `type` values and refuses others with `agent_type_not_allowed`; `when_to_use` and `mode` shape the `task_spawn` description within the existing 2 KiB bound; a `subagent`-mode definition never appears as a selectable primary.
- **AC-09** `--agent` / `default_primary`: the overlay narrows the main role (surface, instructions, model, cap); `agent.selected` is recorded; resume re-applies it and warns on a digest change; plan mode still prevents writes; each error case fails before any model request.
- **AC-10** (slice 5, if D-2 yes) A child receives exactly the MCP tools its `allow_tools` names, after EXT-01 filters; none otherwise; calls are classified and asked as the parent's; an oversize selection is rejected at spawn.
- **AC-11** The doc-consistency tests for the command table pass; `rapid agents scaffold --format md` output validates.
- **AC-12** Revert cycle (S8) for every AC; Windows runs the parsing and surface tests.

## 8. Slices

1. **Markdown form** — shared frontmatter parser, loader, validator unification, CLI, symlink hardening, trust check on `list`/`validate`. AC-01…04, 11.
2. **Narrowing** — `allow_tools`, `deny_tools`, `permission`, `max_steps`. AC-05…07.
3. **Delegation steering** — `mode`, `when_to_use`, `spawn`, `description`. AC-08.
4. **Primary agent** — `--agent`, config default, `agent.selected`. AC-09.
5. **MCP for children** (conditional on D-2) — AC-10.

## 9. Out of scope

- Raising the nested depth above 1 (decision D-3) and `DELEGATION_MAX_DEPTH` reconciliation beyond a documentation fix.
- Replacing a base role's prompt, overriding built-in definitions, or `allow` rules in agent files.
- Mid-session agent switching; per-agent keybindings; agent-defined hooks.
- Loading the repository's root `agents/*.md` profiles at run time.
- A model-visible tool that creates definitions.

## 10. Decisions needed

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D-1 | Frontmatter parser | (a) move the skills loader's closed YAML subset into a shared module; (b) TOML frontmatter between `+++` lines; (c) add a YAML dependency | (a): one tested parser, the same file shape users know from the skill format, no new dependency. |
| D-2 | MCP tools for children | yes behind `allow_tools` (slice 5); no | Yes, only as specified: default none keeps today's behavior and a definition must name what it wants. Needs EXT-01 slice 1 first. |
| D-3 | Nested spawning | keep depth 1; raise to the unused constant 3 | Keep 1. A `spawn` allowlist is meaningful for primary agents now and becomes meaningful for children only if depth is raised by its own spec. |
| D-4 | Symlinked `agents/` directory | skip with a reason; keep following | Skip: every other loader refuses symlinks. Visible change only for someone who relies on a symlinked directory; call it out. |
| D-5 | Step-cap stop reason | reuse `budget_exhausted` (detail `steps`); add `step_limit` | Reuse: adding a token touches the hooks reference, the SDK and every consumer of stop reasons (S11). |
| D-6 | Resume with a changed primary definition | use the current file with a warning; pin the recorded digest and refuse; copy the definition into the session | Current file with a warning; pinning makes a fixed typo unrecoverable without a new session. |

## 11. Contract surfaces to update

A new `docs/reference/agent-definitions.md`; `docs/reference/cli-command-reference.md` (agents rows 37-38, `--agent`); `docs/architecture/agent-harness.md` (definition model, narrowing); ADR 0023 §5 (overlays) cross-referenced by a short ADR 0026; `docs/reference/event-catalog.md`; `sdk/typescript/schemas/wire.v1.json` and regeneration; `crates/tui` for the `/agents` rows; and the stale `USER_DEFS_DIR` constant (`AD:40`) corrected to the real path. Tests to extend: the agent-definition tests in `AD`, the task_spawn tests in `ET`, and `apps/rapid/tests` for the CLI.
