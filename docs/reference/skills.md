# Skills

A skill is a folder holding a `SKILL.md`: a named, described procedure the model
(or you) can pull into a conversation when it is relevant, instead of every
procedure being in every prompt. A skill is **instructions, never privilege**:
loading one adds text to the conversation and nothing else. Every tool call the
text leads to is decided by the permission lattice exactly as if you had typed
the request, a skill cannot carry a permission, and a frontmatter key that would
grant something is refused when the skill is read.

## Where skills are found

| Rank | Root | Path | When |
|---|---|---|---|
| 1 | User | `<RAPIDLM_HOME>/skills/<id>/SKILL.md` (`~/.rapidlm/skills` by default) | always |
| 2 | Extra | `<dir>/<id>/SKILL.md` for each `[skills] extra_dirs` entry | always |
| 3 | Project | `<project>/.rapidlm/skills/<id>/SKILL.md` | only for a **trusted** project (`rapid trust grant`) |
| 4 | Project convention | `<project>/.agents/skills/<id>/SKILL.md` | the same trust gate |

The layout is one level deep: `<root>/<id>/SKILL.md`, no recursion. A skill of
the same id in two roots is found once, in the **higher-ranked** root (the user's
own over the project's, so a repository cannot replace a procedure you
installed); the other is *hidden*, and `rapid skills list` and `rapid doctor` say
so. Subagents (`task_spawn`) are not given the `skill` tool.

`[skills] extra_dirs = ["~/shared-skills", …]` in the user config (`config.toml`
in the RapidLM home, or the file `RAPIDLM_CONFIG` names) names directories that
hold skill folders themselves. At most eight are read; `~` and `~/…` are
expanded; each must exist and be a directory; none may be inside the user skills
directory (it is read already) or be listed twice. An entry that fails any of
that is named, with the reason, by `rapid skills list`, `rapid doctor` and a
line when a session starts, and the rest are still read.

**A skill is refused**, not silently dropped, and said so with a reason:
`name_mismatch` (the `name` in the file is not its folder's name),
`privilege_injection` (a key that would grant something), `too_large`, `symlink`
(the folder or its `SKILL.md` is a link — links are never followed),
`invalid_name`, `unreadable`, or `malformed` (the frontmatter does not parse).
A folder with no `SKILL.md`, and a file in a skills directory, are not skills and
are not reported. Past 64 skills the first 64 by rank and name are kept and the
rest are counted; a directory with more than 256 entries is read to that cap.

`rapid skills list` shows every skill, which root it came from, and who may
invoke it, then what is hidden, what was refused and why, and where the roots
are; `rapid skills show <id>` prints one skill's description, root, flags,
resources and its whole instructions. `rapid skills discover [--roots <path>…]`
looks under your home and configuration directories (or `RAPIDLM_CONFIG_ROOTS`,
or `--roots`) for directories whose every folder holds a valid `SKILL.md`, by
shape and not by name, and prints the `extra_dirs` line that would read one. All
three only read.

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
| `name` | the skill's id; required, and the name of its folder |
| `description` | one line shown in the listing; required, at most 1 KiB |
| `user_invocable` | may a person load it with `/skill`? default `true` |
| `model_invocable` | may the model load it with the `skill` tool? default `true` |
| `resources` | files next to `SKILL.md` loaded with it as prompt text (at most 16) |
| `arguments` | names for the first words of the arguments, at most nine; see below |
| `paths` | globs that would auto-select the skill; not used by the session yet |
| `allowed-tools` | descriptive only; it grants nothing |
| `update` | an upstream pointer; descriptive only |

The key set is closed: an unknown key, and any key that would grant something
(`tools`, `permission`, `permissions`, `hooks`, `mcp`, `env`, `run`, …), makes
the skill unreadable. A skill is dropped if it does not parse. Bounds: 64 KiB per
file, 64 skills, 256 directory entries.

## Arguments

The text after `/skill <id>`, `--skill-args`, or the `skill` tool's `arguments` is
filled into the skill's text with the grammar custom commands use:

| In the text | Becomes |
|---|---|
| `$ARGUMENTS` | the whole argument string |
| `$1` … `$9` | its words, split as a shell splits them (quotes group, a backslash escapes) |
| `$<name>` | the word at the position of a name the skill lists under `arguments:` |
| `$$` | a literal `$` |

```markdown
---
name: release
description: Cut a release
arguments:
  - version
  - channel
---
Cut release $version on $channel (everything: $ARGUMENTS).
```

A name starts with a lower-case letter, then `a`–`z`, digits or `_` (at most 64 bytes;
`arguments` is taken), is listed once, and there are at most nine, in the order of the
words: `$version` is `$1`. A list on one line (`arguments: version channel`) works too. A
list that breaks any of this makes the skill refused as `malformed`. Only a whole identifier is a name
(`$versions` is literal text unless declared), and a missing word is empty. Any other
`$word` stays as written. Nothing here runs a command or reads a variable or a file: the
arguments are text, and a skill that wanted them to do more would still go through the
permission lattice like any other text. Arguments that no placeholder put into the text
that is kept (the skill names none, or only past the 16 KiB cut) follow the text, as
before. The digest of the skill is of its text as written, with the placeholders in it.

## What the model sees

- **The listing.** When at least one skill has `model_invocable` true, the compiled
  context gains a block (locator `skills/selected`) with one line per such skill,
  `- <id> — <description cut to 200 bytes>`, at most 4 KiB in all; what does not
  fit is summarised as `+N more — use rapid skills list`. With no skill there is
  no block and no `skill` tool: nothing changes.
- **The `skill` tool.** `skill {name, arguments?}` loads a listed, model-invocable
  skill. It is read-only and works in every permission mode, plan mode included.
  The result is the text between `[skill:<id> source=<user|extra|project|plugin> sha256=…]`
  and `[/skill:<id>]`; the skill's own text over 16 KiB (about 4 000 tokens) is cut
  at a line with `[skill truncated]`, and `sha256` and the recorded `body_digest`
  are of the whole skill, arguments excluded. `arguments` (at most 4 KiB, no NUL)
  fill the skill's placeholders (next section); where no placeholder lands in the
  text that is kept they follow it, after any cut, so a long skill never loses them. An
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
  again when the turn has finished) and a `user_prompt_submit` hook may block it.
  The skill is recorded and counted as loaded just before its message goes out, and
  only once those checks have passed; if the turn then fails to start (a conflict
  the retries did not clear) the claim is given back and the record stays. Moving to
  another conversation (`/undo`, `/fork`, `/rewind`, `/resume`) forgets what you had
  loaded. The
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

`skill(...)` permission rules with managed limits are the later slice of
`docs/goals/ext-surface/EXT-04-skills.md`. Skills namespaced
by plugin (`<plugin>:<skill>`) wait for a plugin that carries skills: there is
none.
