# Skills

A skill is a folder holding a `SKILL.md`: a named, described procedure the model
(or you) can pull into a conversation when it is relevant, instead of every
procedure being in every prompt. A skill is **instructions, never privilege**:
loading one adds text to the conversation and nothing else. Every tool call the
text leads to is decided by the permission lattice exactly as if you had typed
the request, a skill cannot carry a permission, and a frontmatter key that would
grant something is refused when the skill is read.

## Where skills are found

| Root | Path | When |
|---|---|---|
| User | `<RAPIDLM_HOME>/skills/<id>/SKILL.md` (`~/.rapidlm/skills` by default) | always |
| Project | `<project>/.rapidlm/skills/<id>/SKILL.md` | only for a **trusted** project (`rapid trust grant`) |

The layout is one level deep: `<root>/<id>/SKILL.md`, no recursion. A skill with
the same id in both roots is found once, and **the user's wins**. Subagents
(`task_spawn`) are not given the `skill` tool.

`rapid skills list` shows every skill, which root it came from, and who may
invoke it; `rapid skills show <id>` prints one skill's description, root, flags,
resources and its whole instructions. Both only read.

## The file

```markdown
---
name: release
description: Cut a release branch and tag it
user_invocable: true
model_invocable: true
resources:
  - checklist.md
---
Prepare the release. Follow the checklist…
```

| Key | Meaning |
|---|---|
| `name` | the skill's id; required |
| `description` | one line shown in the listing; required, at most 1 KiB |
| `user_invocable` | may a person load it with `/skill`? default `true` |
| `model_invocable` | may the model load it with the `skill` tool? default `true` |
| `resources` | files next to `SKILL.md` loaded with it as prompt text (at most 16) |
| `paths` | globs that would auto-select the skill; not used by the session yet |
| `allowed-tools` | descriptive only; it grants nothing |
| `update` | an upstream pointer; descriptive only |

The key set is closed: an unknown key, and any key that would grant something
(`tools`, `permission`, `permissions`, `hooks`, `mcp`, `env`, `run`, …), makes
the skill unreadable. A skill is dropped if it does not parse. Bounds: 64 KiB per
file, 64 skills, 256 directory entries.

## What the model sees

- **The listing.** When at least one skill has `model_invocable` true, the compiled
  context gains a block (locator `skills/selected`) with one line per such skill,
  `- <id> — <description cut to 200 bytes>`, at most 4 KiB in all; what does not
  fit is summarised as `+N more — use rapid skills list`. With no skill there is
  no block and no `skill` tool: nothing changes.
- **The `skill` tool.** `skill {name, arguments?}` loads a listed, model-invocable
  skill. It is read-only and works in every permission mode, plan mode included.
  The result is the text between `[skill:<id> source=<user|project> sha256=…]`
  and `[/skill:<id>]`; the skill's own text over 16 KiB (about 4 000 tokens) is cut
  at a line with `[skill truncated]`, and `sha256` and the recorded `body_digest`
  are of the whole skill, arguments excluded. `arguments` (at most 4 KiB, no NUL)
  follow the skill's text, after any cut, so a long skill never loses them. An
  unknown name fails as `skill_unknown` with up to five near matches. The tool is
  offered only where an activation can be recorded in the session ledger (not in a
  workflow step), and a subagent is given neither the tool nor the listing.
- **Once per context.** Asking for a skill that is already in front of the model
  answers `already active (turn N)`, unless the call gives arguments other than
  the ones it was loaded with, which load it again. What the model loaded lasts the
  turn (a tool result does not outlive its turn); what you loaded with `/skill` or
  `--skill` is part of your message and lasts until the conversation is compacted,
  after which the model may load it again.

## What you can do

- `/skill <id> [arguments]` in the terminal UI loads a skill as your own message
  (Tab completes ids you may invoke, capitals included). Bare `/skill` lists them.
  The arguments are taken as typed. It is not queued behind a running turn (type it
  again when the turn has finished), a `user_prompt_submit` hook may block it, and
  nothing is recorded or counted as loaded for a message that is not sent. The
  `@path` references in a skill's text, and in the arguments, attach nothing; the
  ones you type outside it do. A long message is cut for display in the transcript
  and the model gets all of it. A custom command named `skill` is shadowed by the
  built-in (`/commands` says so).
- `rapid exec --skill <id> [--skill-args "<text>"] [prompt]` does the same for a
  headless run; the prompt may be omitted. An unknown id exits with the usage
  code before any model request. `--skill` and `--skill-args` are flags, so an
  unquoted prompt cannot contain those words. The activation is recorded as soon
  as the turn has started, before any model step.
- `rapid doctor` has a `skills` row: the count, and whether a project's skills
  were left out because the project is not trusted.

## The record

Each activation appends `skill.activated` to the session ledger before the text
reaches the model (`rapidlm.skill.activated/v1`: `id`, `root_class`, `body_digest`,
`args_digest` when arguments were given, `origin` `model` or `user`, `generation`,
filed under the `tool` family). The terminal UI shows `[skill <id> activated]`.
Digests, not text: the instructions are in the conversation, the ledger says that
they entered and which.

## Not yet

Further roots (`.agents/skills`, `[skills] extra_dirs`, `rapid skills discover`),
shadowed and rejected skills in the listing, argument placeholders, and
`skill(...)` permission rules with managed limits are the later slices of
`docs/goals/ext-surface/EXT-04-skills.md`.
