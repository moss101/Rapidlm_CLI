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

`[skills] extra_dirs = ["~/shared-skills", …]` in the user config (the file
`RAPIDLM_CONFIG` names when it is set and absolute, else `config.toml` in the
RapidLM home when it is there, else where the home conventions put it — the file
the other commands read) names directories that hold skill folders themselves.
Each entry is an absolute path or starts with `~` or `~/` (a relative one would
mean whatever the working directory holds, a repository's own files included, so
it is refused) and names an existing directory; none may be the user skills directory itself (it is read
already; a folder *inside* it is a different root, since each root is read one
level deep) or be listed twice. At most eight that can be read are read: an entry
that cannot be does not use up a place, and only the first 32 entries are looked at
(a config over 256 KiB is not read for this). An entry that fails is named, with the
reason, by `rapid skills list`, `rapid doctor` and a line when a session starts,
and the rest are still read. A running session follows a change to the list at
its next turn; what you had loaded with `/skill` stays loaded.

**A skill is refused**, not silently dropped, and said so with a reason:
`name_mismatch` (the `name` in the file is not its folder's name),
`privilege_injection` (a key that would grant something), `too_large`, `symlink`
(the folder or its `SKILL.md` is a link — links are never followed),
`invalid_name`, `unreadable`, or `malformed` (the frontmatter does not parse).
A folder with no `SKILL.md` (whatever it is named), a file in a skills directory,
and a link that could not have been a skill (its name is not a valid id, or it
points at a file) are not skills and are not reported; a link to a directory, or to
nothing, is reported as `symlink`. Past 64
skills the first 64 by rank and name are kept and the rest are counted; a
directory with more than 256 entries is read to that cap, in the order the
filesystem lists them, and `rapid skills list` and `rapid doctor` name the
directory (the session line counts them).

`rapid skills list` shows every skill, which root it came from, and who may
invoke it, then what is hidden, what was refused and why, and where the roots
are; `rapid skills show <id>` prints one skill's description, root, flags,
resources and its whole instructions. `rapid skills discover [--roots <path>…]`
looks under your home and configuration directories (or `RAPIDLM_CONFIG_ROOTS`,
or `--roots`) for directories whose every folder holds a valid `SKILL.md`, by
shape and not by name, and prints the `extra_dirs` line that would read one. A
directory that is a project's own skills directory (`.rapidlm/skills` or
`.agents/skills`, in any case, and wherever a link to it lies) is shown but not
offered: it is read for a trusted project, and through `extra_dirs` it would be
read for any. Your own `~/.agents/skills` is yours and is offered. The line it
prints names each directory by where it really is. All three only read: they do not
create the RapidLM home if it is not there yet.

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
the skill unreadable. A skill that does not parse is refused and listed. Bounds: 64 KiB per
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
words: `$version` is `$1`. Write them as a block list, or on one line separated by
spaces (`arguments: version channel`; `[a, b]` and commas are not read). A list that
breaks any of this makes the skill refused as `malformed`.

Only a whole identifier is a name: `$versions`, `$versionX`, `$version2` and
`$version_` are literal text (ASCII letters, digits and `_` after a name make a longer
word; any other character, `.` or `-` or a letter of another alphabet, ends it), and
so is a name nobody declared. `$ARGUMENTS` and `$1`…`$9` are recognised wherever they
appear: `$ARGUMENTSX` is the arguments followed by an `X`, and `$10` is `$1` followed
by a `0`. A missing word is empty. Any other `$word` stays as written. Nothing here runs a
command or reads a variable or a file: the arguments are text, and a skill that wanted them
to do more would still go through the permission lattice like any other text. Only the
skill's own text is filled in: a resource file is added as it is, `$` and all. The digest
of the skill is of its text as written, with the placeholders in it.

**When the text is filled in.** A skill that declares `arguments:` is, so is any
activation that gives arguments, and so is a skill whose text contains `$ARGUMENTS`
(it asks for them, and gets nothing where none were given, as a custom command
does). Arguments that are only blanks are none, whichever way they were passed. A
skill that declares none, is given none and does not ask is inserted as written, so
a skill that carries a shell snippet (`awk '{print $1}'`, `$$`) is unchanged by this
feature until someone gives it arguments. (As written still means: trailing blank
lines are trimmed, a line that would close the block is defused, and the text is cut
at 16 KiB.)

**Where the arguments end up.** `$ARGUMENTS` stands for the whole string. If the skill has
one and it is inside the part of the text that is kept, nothing is added. A skill that only
takes words by position (`$1`, `$version`) has chosen which words it wants; the first such
placeholder decides. In both cases, if that placeholder lies past the 16 KiB cut, or the cut
falls inside the value it was filled with, the whole argument string follows the text
instead, so a long skill never loses them. A skill with no placeholder at all has them
after its text.

## What the model sees

- **The listing.** When at least one skill has `model_invocable` true, the compiled
  context gains a block (locator `skills/selected`) with one line per such skill,
  `- <id> — <description cut to 200 bytes>`, at most 4 KiB in all; what does not
  fit is summarised as `+N more — use rapid skills list`. With no skill there is
  no block and no `skill` tool: nothing changes.
- **The `skill` tool.** `skill {name, arguments?}` loads a listed, model-invocable
  skill. It is read-only and works in every permission mode, plan mode included
  (a `skill(...)` rule can still refuse it or ask first; see Rules and limits).
  The result is the text between `[skill:<id> source=<user|extra|project|plugin> sha256=…]`
  and `[/skill:<id>]`; the skill's own text over 16 KiB (about 4 000 tokens) is cut
  at a line with `[skill truncated]` (also shown where filling in the arguments
  would have added more than blanks past that bound), and `sha256` and the recorded `body_digest`
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
- `rapid doctor` has a `skills` row: the count, how many are hidden by a
  higher-ranked root or refused, the directories that hold more entries than are
  read, the `extra_dirs` entries that were ignored, what the policy refuses, and
  whether a project's skills were left out because the project is not trusted or
  the managed policy turns them off. It warns, with the way out, when something was
  refused, ignored or left out; skills merely hidden by a higher-ranked root are
  counted and are not a warning.

## The record

Each activation appends `skill.activated` to the session ledger before the text
reaches the model (`rapidlm.skill.activated/v1`: `id`, `root_class`, `body_digest`,
`args_digest` when arguments were given, `origin` `model` or `user`, `generation`,
filed under the `tool` family). The terminal UI shows `[skill <id> activated]`.
Digests, not text: the instructions are in the conversation, the ledger says that
they entered and which.

## Rules and limits

Who may load which skill is decided by rules *about* skills, written by you, your
project or your organisation. A skill cannot carry one: a frontmatter key that
would grant something is refused. Policy only narrows.

**Permission rules.** `skill` is a tool name in the permission rules, written in
lower case, with the skill's id as the pattern:

```json
{ "permissions": { "ask": ["skill(release)"], "deny": ["skill(deploy*)"] } }
```

- `deny` hides the skill from the listing and from `/skill` completion, and refuses
  it — to the model **and** to a person who types its name, so a managed deny binds
  you too. A typed `/skill`, `rapid exec --skill <id>` and a model call refused by
  the managed `denied_ids` get the typed failure `skill_denied`; a model call
  refused by a *rule* is answered by the tool runner's denial, as for any tool.
  Either way nothing is read or recorded. `rapid skills list` names what is refused
  and by what; `rapid skills show <id>` says it; `rapid exec --skill <id>` exits
  with the policy code before any model call.
- `ask` makes the **model's** load wait for an approval (the request reads
  `load skill <id>` with the arguments it passed, scope `skill:<id>`; "remember" is
  not offered, since a rule outranks any grant). A person typing the name has
  already asked, so `/skill` and `--skill` are not asked. A headless run has no one
  to ask, so there an `ask` rule is a denial.
- `allow` is the default and changes nothing.

Patterns are matched against the id with `*` and `?` as in every rule, and without
regard to case (a folder `Deploy` and one called `deploy` are the same folder on some
filesystems). The rules come from the permission settings as for any tool: a trusted
project's settings files (`.rapidlm/settings.json`, then the compatibility file), the
managed `denied_tools`, and your own "never allow" answers. A project that is not
trusted contributes no rule. The turn's permission settings are read relative to the
directory `rapid` was started in, as for every rule of every tool, so start it at the
project root: from a subdirectory a `deny` is still enforced for skills (they are read
from the project root) but an `ask` is not raised.

**Managed policy.** The `[skills]` table of the managed policy file (beside
`[policy]`; closed, so a key it does not know refuses the policy):

```toml
[skills]
denied_ids = ["deploy*"]    # id globs refused whatever any rule says; any case
project_skills = false      # a project's own skills are not read at all
```

`project_skills = false` is the organisation turning off the text a repository would
otherwise put in front of the model; your own skills and the `extra_dirs` outside the
project are unchanged, but an `extra_dirs` entry that lies inside the project is not
read either (it would be the project's text by another door) and is named as ignored.
`rapid skills list` and `rapid doctor` say when it is in force. Refusals by the managed
policy change only in the managed policy file. A managed policy that cannot be read
cannot say what it refuses, so **no** skill is loaded until it can be read (a turn
would fail on the same policy), and the reason is shown.

**Bounds.** In the user config:

```toml
[skills]
max_model_activations_per_turn    = 8     # the default
max_model_activations_per_session = 64    # the default
```

At the bound the model's next load is refused as `skill_limit` (it is told a person
can load more with `/skill`). Whole numbers from 0 to 1000; `0` means the model may
load none by itself. Only a load that happens counts: asking for a skill that is
already in front of the model, a refusal, a skill that could not be read and a skill
you load with `/skill` are not counted. `rapid skills list` shows the bounds in force.

Skills namespaced by plugin (`<plugin>:<skill>`) wait for a plugin that carries
skills: there is none.
