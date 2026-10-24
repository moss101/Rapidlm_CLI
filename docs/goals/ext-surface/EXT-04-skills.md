# EXT-04 — Skills: reach the existing loader from a session, add discovery roots, arguments and rules

**Status:** draft behavior spec (not yet scheduled; see [README](README.md))  
**Size:** M (four slices; slice 0 is "wire it" and is most of the user-visible value)  
**Depends on:** nothing. Shares a frontmatter parser and an argument expander with [EXT-05](EXT-05-agent-definitions-markdown.md) (README, shared work).  
**Invariants touched:** core 10, 12, 13; SEAMS S1, S3, S4, S5, S7, S9, S11  
**Baseline:** `05ad9d6`. Every "Today" line is a reading of that tree and must be rechecked at Phase 0 (SEAMS §5.1); lines marked *(spot-checked)* were re-read by the author of this spec. `SK` = `crates/plugin-host/src/skills.rs`, `CC` = `apps/rapid/src/custom_commands.rs`.

---

## 1. Intent

A skill is a folder holding a `SKILL.md`: a named, described piece of procedure the model (or the user) can pull into a conversation when it is relevant, instead of carrying every procedure in every prompt. RapidLM has a careful loader for them and no way to reach it. This spec:

- **Slice 0** connects the loader to sessions: the model sees a short list of available skills and can activate one; the user can activate one by name; `rapid skills` exists.
- **Slice 1** adds discovery roots and makes rejection visible.
- **Slice 2** lets an activation carry arguments.
- **Slice 3** lets rules govern which skills may be used and bounds how many a turn may activate.

Per SEAMS S7 the expected work is mostly "reach the crate from the loop", not "build a loader" — with one loader change the audit found: a model-initiated activation by name does not exist yet (see 3.0).

## 2. Today

- **The loader exists and is complete.** `SkillRoots` (`SK:256`), `SkillLimits` (`SK:265`), `discover` (`SK:651`), `activate` (`SK:694`), `activate_matching` (`SK:708`), `parse_skill` (`SK:742`). Roots are `<user>/skills`, `<marketplace>/skills` and `<project>/.rapidlm/skills`, the last only when the project is trusted (`SK:36-42, 655-689`). Layout is `<root>/<id>/SKILL.md`, one level deep, no recursion (`SK:820-892`).
- **Format.** A closed, hand-rolled YAML subset. Keys: `name`, `description`, `paths`, `user_invocable`, `model_invocable` (hyphen aliases accepted), `resources`, `allowed-tools`, `update` (`SK:86-98`). Inline `[a, b]` lists are rejected (`SK:1427`). **`user_invocable` and `model_invocable` default to true** *(spot-checked, `SK:802-803`)*. Auto-activation additionally needs a non-empty `paths` list and a glob hit (`SK:573-581`). Keys that would grant something (`tools`, `permission`, `permissions`, `hooks`, `mcp`, `env`, `run`, …) fail closed as `PrivilegeInjection`, checked before unknown-field (`SK:99-135, 1360-1363`). `allowed-tools` is descriptive only (`SK:558`).
- **Bounds.** 64 KiB per file, description 1 KiB, 16 resources, 32 paths, 16 allowed-tools, 64 skills, 256 directory entries, a 4 000-token budget, a 1 s timeout (`SK:45-75`).
- **Precedence.** Sorted by id, then User < Marketplace < Project, first wins — so a **user skill beats a project skill of the same id** and the loser disappears silently (`SK:1018-1050`; test `SK:2094`). Custom commands and agent definitions are the other way round: project over user (`CC:132`, `agent_defs.rs:807`).
- **Failure handling.** A skill that fails to parse is **dropped silently**, with no list of what was skipped (`SK:885-891`). Exceeding the skill-count or directory-entry cap fails the **whole** `discover` with `TooManySkills` (`SK:841-843, 1038-1040`). `name` is never compared with the directory name (`SK:894-921`).
- **It is not reachable.** *(spot-checked: no caller of `SkillRoots`, `SkillLoader`, `skills::discover` or `plugin_host::skills` exists under `apps/rapid/src`.)* `apps/rapid` uses `plugin-host` for manifests, trust and hooks only (`p9_commands.rs:2924-3300`, `doctor.rs:1522`). There is no model-visible skill tool (16 built-in tools, `exec_tools.rs:10938-11183`), no listing in the compiled prompt (`crates/agent-runtime/src/prompt_stack.rs:173`), no `/skill` command (`crates/tui/src/commands.rs:602`), and `rapid skills` is not in `SUBCOMMANDS` (`interactive.rs:657-881`); it appears only in the roadmap table of `cli-command-reference.md:47`. The only plumbing is a `skills/selected` context block that production always fills with `String::new()` (`host.rs:2304`, `interactive.rs:4999`).
- **Repository files.** `skill/SKILL.md` and `skill/long-running-goals/SKILL.md` are packaging for *external* hosts, validated only by `apps/rapid/tests/skill_package.rs`. The eight `skills/*.md` files have no frontmatter and are flat, so this loader could never find them. The tracked file is `Skills.md` and it links to `skills.md`, which resolves only on a case-insensitive filesystem (where it is the same file); on a case-sensitive one the link dangles.
- **Custom commands already have an argument grammar.** `$ARGUMENTS`, `$1`…`$9` split as a shell does, `$$`, and "arguments appended after a blank line when the body uses none" (`CC:342-431`; `docs/reference/custom-commands.md`). `expand` reads only `.template` (`CC:395`) and could take a string. Names differ: skill ids allow uppercase and `._:` up to 256 bytes (`SK:1613-1630`); command names are `[a-z0-9_-]` plus `:` (`CC:246`).
- **Docs drift.** `plugins-skills-hooks.md` calls the loader `SkillRegistry`; the code has `SkillLoader`. `SK:35` says the user root is `~/.config/rapidlm/skills`; the real home is `RAPIDLM_HOME` or `~/.rapidlm` (`interactive.rs:2403-2422`).

## 3. Behavior

### 3.0 Slice 0 — wire the loader

**Roots.** Exactly the loader's existing roots: user `<RAPIDLM_HOME>/skills`, the plugin/marketplace `skills` directory, and `<project>/.rapidlm/skills` for a trusted project. Discovery runs when a turn's context is built and its result is cached for the session until the next turn boundary at which a root's directory metadata changed.

**Listing.** The compiled context gains a block `available skills`: one line per skill, `<id> — <description cut to 200 bytes>`, in precedence order, at most 4 KiB in total (overflow is summarised as `+N more — use rapid skills list`). Only skills with `model_invocable` true appear. The block is placed like other project-supplied instruction text, and a skill from the plugin/marketplace root is marked as such in its line; the body of any skill is **prompt data, never a grant** (below).

**Loader change (the one piece of new loader behavior in slice 0).** Today a model-side activation needs a non-empty `paths` list and a glob hit (`SK:573-589`), `ActivationQuery` has no by-name model request, and a named request checks `user_invocable`, never `model_invocable` (`SK:295, 573-589`). Add a by-name request carrying its origin (`user` or `model`): a `user` request requires `user_invocable`, a `model` request requires `model_invocable`; the path-triggered `activate_matching` is unchanged.

**The `skill` tool.** A built-in tool `skill { name, arguments? }`, read-only and session-local, allowed in every permission mode including plan mode (it touches nothing outside the conversation):

- `name` must be a listed, `model_invocable` skill; otherwise a typed failure `skill_unknown` that lists up to five near matches.
- The result is the skill's body (expanded per 3.2), placed in the context as a block with the locator `skill:<id>` using the existing "prompt data only" activation (`SK:693`): it can instruct the model how to work; it cannot widen what the model may do (core invariant 13). Skill resources the loader lists are read through the normal read tool under the normal permissions; activation never reads outside the skill directory.
- **Idempotence.** Activating a skill already active in the current context generation returns `already active (turn <n>)` and does not insert the body again. Compaction starts a new generation.

**User surfaces.**

- `/skill <id> [arguments…]` in the terminal UI, with completion over `user_invocable` skills. A typed activation always inserts the body (an explicit request overrides idempotence).
- `rapid exec --skill <id> [--skill-args "<raw>"] [prompt]`.
- `rapid skills list` — every skill with source root, precedence, the two invocable flags and, separately, **shadowed** skills (same id, lower precedence) and **rejected** skills with the reason. `rapid skills show <id>` prints the parsed frontmatter, the resolved root and the body. Both are read-only and offline. `rapid skills` joins `SUBCOMMANDS` and the doc-consistency test (`interactive.rs:18349`).
- The `enable` / `disable` verbs in the roadmap table are **not** part of this spec (decision D-4).

**Trust.** Project skills load only for a trusted project (unchanged). The doctor row says when skills were skipped for trust.

**Event.** `skill.activated { id, root_class: "user"|"project"|"extra"|"plugin", body_digest, args_digest, origin: "model"|"user", generation }` is appended before the body enters the context. The transcript shows `[skill <id> activated]`.

### 3.1 Slice 1 — roots and visibility

1. **Project convention directory.** `<project>/.agents/skills` (a vendor-neutral directory convention) is a project root at the same trust gate as `.rapidlm/skills`, **below** it in precedence.
2. **Extra roots.** `[skills] extra_dirs = ["~/path", …]` in user `config.toml`: at most 8 directories, each an absolute path or one starting with `~` / `~/` (a relative path would mean whatever the working directory holds), each must exist, none may be `RAPIDLM_HOME/skills` itself (no double roots; a folder *inside* it is another root, since each root is read one level deep). *Implementation note (EXT-04-1-R):* an entry that cannot be read does not use up one of the 8, and only the first 32 entries are examined. Precedence: **user root > extra roots > project roots** (decision D-1; the first version of this list said project > user, which D-1 and §3.1.4 contradict).
3. **No product-named locations.** Per SEAMS §2.1/§2.5 the codebase carries no table mapping a peer product to a skills directory. `rapid skills discover [--roots <path>…]` finds candidates **by shape**: directories (depth ≤ 3) under the home and XDG configuration roots whose immediate subdirectories each contain a `SKILL.md` with valid frontmatter; it lists them by path with a count, mutates nothing, and prints the `extra_dirs` line to add. `RAPIDLM_CONFIG_ROOTS` overrides the scan roots, as `rapid mcp install --discover` does.
4. **Same-id collisions.** Highest precedence wins and every shadowed skill is reported once (list, doctor). **Precedence stays user-over-project** as the loader has it (decision D-1); the change is that the shadowing becomes visible.
5. **Hardening.** `name` must equal the directory name (mismatch is a rejection). Directories and `SKILL.md` files that are symlinks are skipped with a reason. Because nothing reaches this code today, none of these changes affects an existing user (S11).
6. **No silent drops.** Every rejected skill is returned with a typed reason (`PrivilegeInjection`, `NameMismatch`, `TooLarge`, `Malformed`, `Symlink`, …) and shown by `rapid skills list`, the doctor row and, once per session, a one-line notice. Exceeding the 64-skill cap keeps the first 64 by precedence and name and reports the rest, instead of failing the whole discovery.
7. **Plugin skills** are namespaced `<plugin id>:<skill>` so two plugins cannot collide; the id grammar already allows `:`.

### 3.2 Slice 2 — arguments

- **Declaration.** Optional frontmatter `arguments:` — a whitespace-separated list of up to nine names, each `[a-z][a-z0-9_]*` and none equal to `arguments`. (A scalar, because the closed parser rejects inline lists; block lists are also accepted.)
- **When it applies (EXT-04-2-R).** A skill that declares `arguments:` is filled in; so is any activation whose arguments are not blank; so is a skill whose text contains `$ARGUMENTS`. A skill that declares none, is given none and does not ask for them is inserted exactly as written, so a skill that carries a shell snippet (`$1`, `$$`) is unchanged until someone gives it arguments (S11: defaults preserve today's behavior). Arguments that are only blanks are none.
- **Grammar.** The same as custom commands: `$ARGUMENTS` (the raw string), `$1`…`$9` (shell-style splitting: quotes group, a backslash escapes), `$$` (a literal dollar), plus `$<name>` for a declared name (position = declaration order). Any other `$word` is literal. A missing positional is empty. A body using none of these gets the raw arguments appended after a blank line.
- **One implementation.** The expander moves to a shared module taking `(&str body, &[&str] declared_names, &str raw)`; custom commands call it with no names, and their documented behavior and tests do not change.
- **Bounds.** Arguments ≤ 4 KiB, no NUL. Expansion is textual and done by the host: **no** shell, environment, file or command substitution (S4).
- **Idempotence key.** `(skill id, args digest)`; a different argument string is a new activation.

### 3.3 Slice 3 — rules and limits

- **Rule subject.** `skill` becomes a tool name in the permission-rule grammar and is written in the lowercase form every tool name uses — `skill(release*)`, `skill(*)` — with the existing `tool(arg-glob)` form. Tool names are compared exactly (`permissions.rs:172`) with no aliasing, so a capitalised `Skill(...)` would never match. Effects:
  - `deny` hides the skill from the listing and from `/skill` completion and refuses activation with `skill_denied` — for the model **and** for a typed `/skill`, so a managed deny binds the user too.
  - `ask` creates an approval (`source: skill:<id>`, `arguments_digest` = digest of the argument string) for **model-initiated** activations only; the user's own typed activation does not ask.
  - `allow` is the default and changes nothing.
- **A skill never carries its own permissions.** The loader's refusal of `permission(s)` / `tools` / `hooks` / `mcp` / `env` / `run` in frontmatter stays: a rule *about* a skill is written by the user, project or policy, never by the skill (core invariant 13, S5). `allowed-tools` stays descriptive.
- **Managed.** `[skills] denied_ids = [patterns]` and `[skills] project_skills = false`, narrow-only, reported with field, origin and remediation.
- **Limits.** `[skills] max_model_activations_per_turn` (default 8) bounds model-initiated activations; further calls return `skill_limit`. Skills cannot invoke skills — an activation is a tool call — so a nesting depth has no meaning here; the per-turn cap bounds the same risk. `max_model_activations_per_session` (default 64) bounds the session.
- **Subagents** do not receive the `skill` tool in this spec (invariant 12); EXT-05 may offer skills to a definition later.

## 4. Configuration surface

```toml
# ~/.rapidlm/config.toml
[skills]
extra_dirs                       = ["~/shared-skills"]
max_model_activations_per_turn   = 8
max_model_activations_per_session = 64

# managed policy
[skills]
denied_ids      = ["deploy*"]
project_skills  = false
```

```markdown
---
name: release
description: Cut a release branch and tag it
arguments: version channel
model_invocable: true
---
Prepare release $version on the $channel channel. Follow the checklist…
```

```json
{ "permissions": { "ask": ["skill(release)"], "deny": ["skill(deploy*)"] } }
```

## 5. Records and contracts

| Surface | Change |
|---|---|
| Ledger `skill.activated` | as in 3.0; registered like EXT-01 §5 (`EventFamily` has no `Skill` variant — Phase 0 decides between adding one and filing under `Agent`/`Tool`) |
| Tool | `skill` (read-only, session-local); typed failures `skill_unknown`, `skill_denied`, `skill_limit` |
| `rapid skills list|show|discover` | new subcommand; `SUBCOMMANDS`, completions, man page and the doc-consistency test updated |
| Frontmatter | adds `arguments`; no new privilege-bearing key |
| Compiled context | `available skills` block and `skill:<id>` blocks |
| Schemas | `rapidlm.skill_descriptor` (`SK:27`) gets a fixture under `crates/protocol` if the descriptor shape changes |
| Loader | by-name activation request carrying `origin` (3.0); `arguments` added to the closed frontmatter key set (privilege-key denylist unchanged) |

## 6. Failure and edge behavior

- A skill id containing characters a user cannot type as `/skill <id>` is still activatable by the model and listed; completion shows it.
- Two roots containing the same directory through different paths (a root listed twice) are de-duplicated by canonical path.
- An unreadable root is reported and skipped; it never fails the session.
- A skill body larger than the activation's token budget (4 000 tokens, `SK` limits) is cut at a line boundary with a visible `[skill truncated]` marker and the digest covers the full body.
- A race (a skill edited between listing and activation) activates the file as it is and records its digest; the listing is not re-validated until the next turn.
- Headless `rapid exec` can use the `skill` tool and `--skill`; nothing needs a human unless a rule says `ask`, in which case the existing headless approval contract (exit 10) applies.

## 7. Acceptance criteria

- **AC-01** With no skills on disk nothing changes: no `skill` tool is offered, no block is added, all existing tests pass; the tool and block appear only when at least one `model_invocable` skill is discovered.
- **AC-02** A user-root skill and a trusted-project skill are listed in precedence order; an untrusted project's skills are not loaded and the doctor says why.
- **AC-03** The `skill` tool activates a skill: the body appears as a `skill:<id>` block, `skill.activated` is appended first, the transcript marker is present; a second activation in the same generation returns `already active`; after compaction it inserts again.
- **AC-04** The tool works in plan mode and every other mode without prompting; a body containing instructions to run commands does not change any permission decision (negative test).
- **AC-05** `/skill`, `rapid exec --skill` and `rapid skills list|show` work; `rapid skills` is in `SUBCOMMANDS` and the reference-consistency test passes.
- **AC-06** `rapid skills list` reports shadowed and rejected skills with typed reasons, including `NameMismatch`, `PrivilegeInjection`, `Symlink` and `TooLarge`; exceeding 64 skills keeps the first 64 and reports the rest instead of failing.
- **AC-07** `.agents/skills` and `extra_dirs` roots load at the stated precedence; an `extra_dirs` entry that is the user skills directory, a relative path or a missing directory is rejected with a reason; `rapid skills discover` finds a fixture directory by shape only, with a neutral name, and writes nothing.
- **AC-08** Argument expansion: `$ARGUMENTS`, `$1…$9` with quoting and escapes, `$$`, declared `$name`, unknown `$word` literal, append rule, 4 KiB bound, NUL rejection. The same fixtures pass through custom commands unchanged.
- **AC-09** `skill(...)` rules: `deny` hides and refuses (for model and user), `ask` creates an approval for a model activation with the right `source` and digest and not for a typed one, managed `denied_ids` and `project_skills = false` narrow and report.
- **AC-10** `max_model_activations_per_turn` and `_per_session` return `skill_limit` at the bound.
- **AC-11** Frontmatter containing a privilege-bearing key is rejected exactly as today (existing tests unchanged).
- **AC-12** Listing bounds hold: description cut, 4 KiB block cap, `+N more` summary.
- **AC-13** Revert cycle (S8) for every AC; Windows runs the same tests; path-separator and symlink cases have portable fixtures or typed unavailability.

## 8. Slices

0. **Wire** — roots as they are, listing block, `skill` tool, `/skill`, `--skill`, `rapid skills list|show`, `skill.activated`. AC-01…05, 11, 12.
1. **Roots and visibility** — `.agents/skills`, `extra_dirs`, `discover`, collisions, hardening, rejection reports, plugin namespacing. AC-06, 07.
2. **Arguments** — shared expander, `arguments`, idempotence key. AC-08.
3. **Rules and limits** — `skill(...)`, managed keys, caps. AC-09, 10.

## 9. Out of scope

- Skills for subagents; skill-defined hooks, tools or permissions.
- Path-triggered auto-activation (`paths` + `model_invocable`) beyond what `activate_matching` already does; wiring it is a follow-up once the explicit path works.
- `enable` / `disable` verbs; a marketplace UI; remote skill installation.
- The repository's own flat `skills/*.md` documents (they are not skills in this format; see D-3).

## 10. Decisions needed

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D-1 | Same-id precedence | keep user over project (as the loader and its test have it); switch to project over user (like commands, agents, rules) | Keep. It was a deliberate, tested choice, it keeps a repository from silently replacing a workflow the user installed, and it costs nothing now because shadowing becomes visible. Revisit if users ask for repository-pinned procedures. |
| D-2 | Directory convention | `.agents/skills` only; also accept any peer product's directory by name | `.agents/skills` plus `extra_dirs`, with shape-based `discover`. A table of product-named paths violates SEAMS §2.1/§2.5. |
| D-3 | The repository's `skills/*.md` | convert to the directory format with frontmatter; leave as documents | Leave; they are agent-facing documents referenced by `Skills.md`. Fix the case-mismatched `skills.md` link. |
| D-4 | `enable` / `disable` | specify now; defer | Defer; precedence plus `skill(...)` rules already cover "do not use this skill". Remove the verbs from the roadmap table. |
| D-5 | Where the argument expander lives | `agent-runtime`; `protocol`; a new tiny crate | `agent-runtime` (both consumers depend on it); avoid a new crate for ~100 lines. |

## 11. Contract surfaces to update

A new `docs/reference/skills.md`; `docs/reference/custom-commands.md` (shared grammar note); `docs/reference/cli-command-reference.md` (rows 5-11, 47 — the consistency test checks them); `docs/architecture/plugins-skills-hooks.md` (name and reachability), `docs/architecture/prompt-runtime.md` (new blocks), `docs/api-contracts/hooks-plugins-api.md` (`skills.list/load` is not implemented — mark or drop), `Skills.md`, `docs/reference/event-catalog.md`, the SDK catalog; the `SK:35` comment.
