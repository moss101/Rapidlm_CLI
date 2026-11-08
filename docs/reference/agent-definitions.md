# Agent definitions

An agent definition names a kind of agent: a base role, a description the
model sees when it chooses what to delegate, and optionally standing
instructions, model, reasoning effort and declared inputs and outputs. The model
delegates with `task_spawn`; its `type` is the definition's id. A session's main
agent can be one too (`--agent`, below). A definition is **a request to narrow,
never a grant**: the agent still runs under its base role's tool surface, the
permission lattice and the managed policy, and a definition can only remove from
what the role already allows.

## Writing one

A definition is a file in a definitions directory, either `<id>.md` or
`<id>.toml`. Both are read into the same in-memory definition by the same
validator, so a definition written in one form and the same definition written in
the other are the same definition, and each refuses what the other refuses. What
one syntax can say and the other cannot is small: a TOML string can hold any
control character, a Markdown value only a tab or a newline (as the escapes
`\t` and `\n`); a TOML `instructions` keeps its leading and trailing white space,
a Markdown body is trimmed; `inputs`, `outputs` and `tool_classes` on one
line are separated by ASCII white space only (a no-break space is a different,
refused, name); and Markdown, where every value is text, reads a number written
in quotes (`max_steps: "12"`) and an empty `permission: []` where the table form
would refuse the first and has no such spelling for the second. Markdown is the
more lenient side only here, and what it accepts it reads as the table form would.

### Markdown

```markdown
---
name: reviewer
description: Read-only code reviewer
base_role: reviewer
tool_classes: read git
model: review
reasoning_effort: high
inputs: diff
outputs:
  - findings
  - verdict
---
You review changes for correctness and security. Report findings only.
```

Front matter sits between two `---` lines, each a line of its own (`---` and a
line break, with nothing after it); everything after the second one is the
**body**, and the body is the agent's instructions. A leading byte-order
mark and CRLF line ends are accepted (the body is read with LF).

| Key | Meaning |
|---|---|
| `name` | the id. Optional: it defaults to the file name without `.md`, and if it is given it must equal it |
| `description` | required; at most 512 bytes; shown to the model when it picks a type |
| `base_role` | required; the role the child runs as |
| `tool_classes` | the tool classes it keeps, a subset of the role's (`read`, `write`, `exec`, `net`, `browser`, `mobile`, `mcp`, `plugin`, `git`, `secret`). Left out, the child has none |
| `model`, `reasoning_effort` | the child's model (a configured `[model.<id>]`) and effort |
| `inputs`, `outputs` | names the task supplies and the result must carry; at most 16 each |
| `allow_tools`, `deny_tools` | tool-name patterns that narrow the child's tools further (see *Narrowing*) |
| `permission` | rules for the child's calls that can refuse or ask, never allow |
| `max_steps` | the most model steps the child takes, 1 to 32 |
| `mode` | where the agent may be used: `subagent` (the default; offered to `task_spawn`), `primary` (a session's main agent only) or `both` |
| `when_to_use` | one line, at most 256 bytes, added to the agent's entry in the list of types the model chooses from |
| `spawn` | the agent ids a main agent of this definition may delegate to (at most 32) |

The key set is **closed**: any other key is refused, and the refusal says which
key. That includes `instructions` (the body is the instructions), `id`, `role`
and `tools` (use `name`, `base_role` and `tool_classes`), and anything that
would try to replace or template the base prompt (`prompt`, `system_prompt`,
`extends`, `template`): there is no way to replace the base role's prompt. The
body is added after it, in the labelled overlay section, and is at most 8 KiB
(trimmed of the blank lines around it).

A list is written either as one line of names separated by spaces
(`tool_classes: read git`), as a block list of `  - name` lines, or as `[]` for
none. A value may be in double quotes, with `\n`, `\t`, `\"` and `\\` as the only
escapes. This is the same closed subset of YAML the skills loader reads and is
read by the same code; there are no anchors, tags, single quotes, flow lists with
items, nested maps or multi-line values, and a repeated key is refused. A
control character other than tab and newline cannot be written in a value.

### TOML

```toml
schema = "rapidlm.agent_defs.v2"
[agent]
id = "reviewer"
description = "Read-only code reviewer"
base_role = "reviewer"          # `role` is the older name for the same key
tools = ["read", "git"]         # the Markdown form calls this `tool_classes`
instructions = "You review changes for correctness and security."
model = "review"
reasoning_effort = "high"
inputs = ["diff"]
outputs = ["findings", "verdict"]
allow_tools = ["repo_*", "workspace_read"]
deny_tools = ["repo_glob"]
max_steps = 12
mode = "both"
when_to_use = "After a change is complete and before it is committed"
spawn = ["explore"]
[agent.permission]
"workspace_patch(docs/*)" = "ask"
```

The table is closed too: an unknown field is refused. `schema = "rapidlm.agent_defs.v1"`
files keep loading, with the v1 key set; the narrowing keys need `v2` (a v1 file
that uses one is told so). `rapid agents scaffold <id> [--format md|toml]` prints
a valid starter in either form.

## Narrowing

A definition can only take away. The child's tools are

> (the base role's classes ∩ `tool_classes`) ∩ `allow_tools` (if present) − `deny_tools`

- `allow_tools` / `deny_tools` are patterns over the tool names `rapid tools`
  prints: `*` any run of characters, `?` one, nothing else special; letters,
  digits, `_` and `-`; at most 64 patterns of 128 bytes. Absent `allow_tools` adds
  no restriction; an empty list leaves the child no tool. An allow list cannot
  name a tool the classes took away (it gains nothing). A pattern that names no
  tool is not an error — an MCP tool may not be connected yet — but
  `rapid agents validate` warns about it and names the closest tools.
- The surface is enforced twice, as the classes always were: the tool is not
  offered to the agent, and a call to it anyway (a model can name any tool) is
  refused as ``tool_not_allowed_for_agent: `<tool>` is not available to agent
  `<id>` ``, and does nothing. The refusal, for a class the agent lacks as for a
  pattern it leaves out, comes before the permission mode is consulted, so a
  call the agent could never make is never put to a human.
- `permission` maps a rule — `Tool` or `Tool(arg-glob)` with an exact tool name,
  as in the project settings' `permissions` — to `deny` or `ask`. `allow` is
  refused when the file loads: an agent file can restrict, never pre-approve. The
  rules are added to the child's own permission context and no one else's, after
  the `write_scope` of its spawn; `deny` outranks `ask`, a rule outranks the
  permission mode, and the managed policy's bans, checked first, outrank every
  rule. A child has no one to answer an `ask`, so it reads as a refusal there.
  At most 32 rules of 256 bytes; a key that is not a rule the settings would read
  is refused. A session holds at most 128 rules in all: if the session's own rules
  and the definition's do not all fit, the child is not started (and says so)
  rather than dropping some of them, which could have been a `deny`.
- `max_steps` caps the child's model steps in a run (the turn's own ceiling is 32).
  When it is reached — after the current step's calls have run, none abandoned —
  the child stops with the stop reason `budget_exhausted` (detail: `steps`) and the
  parent receives a report with status `budget_exhausted` that says so and lists
  the last of the child's calls with what came of each (a note it was handed, such
  as an earlier report or a message, is not one of them). It is an unfinished
  child: headless its changes are dropped, interactively they are held for review,
  it is recorded as not having finished (also when it was a continuation) and
  there is nothing to continue. A `subagent_stop` hook is told that status. The cap
  is per run: a child continued by a message, or by one queued while it ran, takes
  up to the cap again in that run. A stop with exactly the 256-call ceiling's
  worth of calls made is not told as a step stop, since that ceiling may have
  been the cause; it is the plain failure it always was.

## MCP tools for a subagent

A child has no MCP tools — none of the session's servers' — unless its definition names
them. The names are the `mcp__<server>__<tool>` patterns among its `allow_tools` (patterns with
`*` and `?` as everywhere; a pattern such as `*` that does not start with `mcp__` names none),
and the definition must keep the `mcp` class, so its base role must expose it (`explorer`,
`planner`, `reviewer`, `verifier`, `main`). The child is then offered exactly the tools that
match, less its `deny_tools`, that a connected server lists and the server's own filters
(`docs/reference/mcp.md`) allow, as ordinary entries of its tool list — also when the server is
deferred for the parent, whose loaded set a child neither reads nor changes. A call to any other
MCP tool is refused as `tool_not_allowed_for_agent` (or, for one the server's own filter
disables, as that filter's `mcp_tool_not_enabled`). A pattern is matched against the whole name
`mcp__<server>__<tool>`, as permission rules are: `mcp__docs__*` also matches the tools of a
server called `docs_`. Together they must fit a tool list (20 KiB of definitions and the
provider's tool cap less the built-in tools, counted as the list counts them): if they do not,
the spawn is refused as `too_large` with the numbers, and nothing is given. The calls go through
the session's connection and are classified and asked as the parent's would be, in the child's
permission context — the parent's, narrowed for a subagent: a child has no one to ask, so a call
that would ask needs an allow rule or grant, and that includes a child of a session that
bypasses every ask (to its children that mode is accept-edits, and an MCP call is neither an
edit nor a read). The children of a narrowed agent are held to what it has, so an agent without
the `mcp` class has no child that is given MCP tools. **An MCP tool is outside the guarantees of
the built-in tools' scopes.** The read-only scope, `write_scope`, the write budget and a
worktree's isolation cover the built-in tools; a named MCP tool does what its server does, in
the place the server runs (a filesystem, git or database server writes where it writes). A
child that writes nothing in the workspace can call the MCP tools it named and still cannot
write with the built-in ones. Name only tools you would let the child call.
`rapid agents validate` warns about `mcp__` patterns in a definition that does not keep the
`mcp` class, which would give the child none.

## Steering delegation

The model delegates with `task_spawn`, choosing a `type` from a list the tool
describes. The list is bounded (2 KiB, and the short count of what is left out
after it): each type is its id, its base role and its description, followed by
`; use when:` and its `when_to_use` if it has one, as many as fit, the rest
counted (`and N more (rapid agents list)`). A hint that does not fit is left off
its type before the type is left out. What a file says reaches the model as one
visible line: control characters, characters that split a line and characters no
screen shows (tag characters, variation selectors — but not the two that choose an
emoji's presentation — direction overrides) are gone,
white space folded. A configuration that uses none of these keys produces the
same text as before — for descriptions that were already one visible line.

- `mode: primary` keeps an agent out of that list and out of `task_spawn` (naming
  it is "no agent type"); it is for a session's main agent (see *Running a
  session as an agent*). `subagent` (the default) and `both` are offered.
- `spawn` lists the agents a main agent may delegate to. It is for an agent that
  can be a main agent (`primary` or `both`): on a `subagent` definition a
  non-empty `spawn` is refused when the file loads (a subagent cannot spawn
  further agents). An empty `spawn` is no list. Where a main agent has one,
  `task_spawn` offers those types only, and a spawn of another is refused as
  `agent_type_not_allowed` (a denial, listing what is allowed; a type nobody has
  stays an ordinary failure listing the types it may use). A list that names
  nothing the agent can use offers no type at all and refuses every spawn.
  Continuing a child that already ran (a message to it) is not a spawn and is not
  held to the list. `rapid agents validate` warns about an id that no agent has
  or that is a main agent only, and about a `when_to_use` that is never shown.
- `when_to_use` is one visible line of 1 to 256 bytes: control characters,
  characters that split a line and characters no screen shows are refused when the
  file loads.
- The `description` argument of `task_spawn` (a short label) is no longer dropped:
  it is folded to one clean line of at most 200 characters (a value that is not
  text is no label, not an error), shown in the child's row in `/agents` — in
  front of what the child is doing as it queues, is admitted and gets its
  worktree — and recorded on its `agent.spawned` event as `description`. That
  event also carries `definition_digest`, a SHA-256 of everything the definition
  says (the same in either syntax; a list that is a set — `allow_tools`,
  `deny_tools`, `spawn` — counts once each, in any order; moved by any other
  change to it), so a reader can tell which version of a definition a child ran.

## Running a session as an agent

A definition with `mode: primary` or `both` can be the **main agent** of a
session: the agent the conversation is with, rather than one it delegates to.

```
rapid exec --agent <id> <prompt>      # a headless run
rapid --agent <id>                    # the terminal UI
```

or, for every new session, `[agents] default_primary = "<id>"` in the user config
(`~/.rapidlm/config.toml`). The flag wins over the config. `rapid agents list`
shows what can be chosen.

It is an overlay on the main role, and it only narrows:

- its **instructions** follow the host's system prompt in a section headed `Main
  agent '<id>'`; the host's prompt is never replaced;
- its **tools** are the main role's less what the definition leaves out
  (`tool_classes`, `allow_tools`, `deny_tools`), enforced as for a child (so
  `skill`, `mcp_find` and `mcp_load` are among what a narrowed agent does not
  have; name nothing and nothing is left out);
- its **permission rules** are added to the session's, and can only refuse or ask
  (if they do not all fit beside the rules the session has, the run is refused);
- its **`max_steps`** bounds each run (each prompt, and each turn that resumes
  after an approval);
- its **`spawn`** list, if it has one, holds `task_spawn` to those agents;
- its **model** and **reasoning effort** are used where the managed policy
  allows, under the floors that hold over an effort (the reminders' and the
  policy's); a model the user picks with `/model select` wins over the agent's.

Plan mode is a permission mode and is independent: an agent in plan mode cannot
write. The subagents it spawns are not the agent — each runs as the definition of
its own type — but **they are held to what the agent has**: a child has the tool
classes of its type that the agent also has, and passes the agent's `allow_tools`
and `deny_tools` as well as its own. An agent narrowed to `read` has no child that
writes or runs a command, whatever type it spawns, and no child opens a worktree
it cannot write in. An agent that is not narrowed binds its children to nothing,
as always.

**The agent is fixed for the session, and belongs to it.** It is recorded as an
`agent.selected` event (`id`, `definition_digest`, `mode`) as soon as the session
exists — a prompt a hook then blocks still leaves a session that resumes as the
agent. A session that is resumed (`--resume`, `--continue`, `rapid resume`,
`/resume`) runs as the agent it recorded, read from the file as it is now; a fork
(`/fork`, `/undo`, `/rewind`) is the agent its parent was where it branched. A
different `--agent` for a session that has one is refused, and so is one for a
session that began without an agent (start a new session). The configured
default applies to sessions that begin after it was set. If the definition has
changed since it was recorded, the resume says so in one line, uses the current
file and records the new fingerprint. A terminal UI session says `running as
agent '<id>'` when it starts or moves to a session that has one, and moving to a
session with none makes the main role the main role again. Every turn of such a
session runs as the agent whichever surface drives it — the terminal UI, ACP, the
daemon, a turn resumed after an approval; one whose agent can no longer be run as
(its definition is gone, or has become a subagent only) fails with the reason
instead of running as the main role.

These are refused **before any model request**: an id no agent has (the message
lists the agents a session can run as, and `rapid agents list` shows them all); an
agent that is a subagent only; an agent the project defines when the project is
not trusted (run `rapid trust grant` after reviewing it); a model the definition
names that is not configured or that the managed policy does not allow; permission
rules that do not fit beside the session's (remove some); a switch of agent for a
session that has one. `rapid exec` exits 2 for a request that cannot be met and 3
for a refusal of policy; the terminal UI, which has no exit code of its own for
it, exits 2. (A worktree made for `rapid exec --worktree` before a later refusal is
left, as it is for the other refusals that come after it.)

Not covered: a workflow step is the plain main role; `/aside`, the memory
helpers and `/compact` (whose summary uses the session's default model, not the
agent's) are helpers of the session, not turns of the agent.

## Where definitions are found

| Rank | Directory | When |
|---|---|---|
| 1 | built-in (`general-purpose`, `plan`, `explore`, `patch`) | always; cannot be overridden |
| 2 | `<project>/.rapidlm/agents/` | only for a **trusted** project (`rapid trust grant`) |
| 3 | `<RAPIDLM_HOME>/agents/` (`~/.rapidlm/agents` by default) | always |
| 4 | each directory `[agents] extra_dirs` names | always |

A definition of an id that a higher rank holds is **refused**, not merged, and
`rapid agents list` says which it lost to. A project definition that uses a
built-in's id fails its whole directory, as it always has.

`[agents] extra_dirs = ["~/shared-agents", …]` in the **user** config (a project
cannot name a directory for its own definitions to come from) follows the same
rules as `[skills] extra_dirs`: each entry is an absolute path or starts with `~`
or `~/`, names an existing directory, is not the user agents directory itself and
is not listed twice; at most eight are read; an entry that cannot be read is
named, with the reason, by `rapid agents list`.

Rules for a directory:

- Only `*.md` and `*.toml` files directly inside it are read (no recursion), at
  most 128, each at most 64 KiB and valid UTF-8 without a NUL byte.
- A file that is a symlink is refused, and **so is a directory that is a
  symlink**: the loader does not follow either. For a trusted project's directory
  this goes further: it is read only if, with every link on the way followed, it
  is still inside the project (a repository that commits `.rapidlm` as a link to
  somewhere else is one refusal, and nothing is read from there). The same rule
  applies to your own `<home>/agents`: if it is a link (a dotfiles manager makes
  one), name its target in `[agents] extra_dirs`, which is followed.
- If `<id>.md` and `<id>.toml` both exist, **both** are refused, each reason
  naming both files, until one is removed (so a notes file `reviewer.md` beside
  `reviewer.toml` disables the table definition until it is moved).
- A refused file never hides the rest. A Markdown file whose name is not a valid
  id (`README.md`) is refused like any other; keep notes outside the directory.

The refusal reasons are: an unknown key; `name` not equal to the file name; no
front matter, or front matter outside the subset (with its line number); a
description or body over its cap; an unknown role or tool class; a tool class
outside the base role's surface (the message says what the role exposes); a
class this host cannot back; a duplicate id; a built-in's id; a symlink; a
two-syntax collision; a file over the size cap or not UTF-8.

## Commands

```
rapid agents list [--dir <path>]
rapid agents validate [--dir <path>]
rapid agents scaffold <id> [--format md|toml]
```

`list` prints a header (`schema=… dir=… user_dir=… extra_dirs=N loaded=N
rejected=N`), a `project:` line when the project's directory is not read (an
untrusted project, or a link out of the project) and an `extra_dirs:` line for
each entry that could not be read, then every definition that loaded (a spawn can
name the ones whose `mode` allows it): for
each type its id, file format (`md`, `toml` or `builtin`), base role, source
(`builtin`, `project:`, `user:` or `extra:` and the path), tool classes, then —
only where a definition narrows — `allow_tools=`, `deny_tools=`, `rules=N` and
`max_steps=N`, `mode=` (when it is not `subagent`) and `spawn=`, and its model,
effort, inputs, outputs and description, then each refused file with its reason.
Anything a file said is shown on one line — control characters and
characters that split or reorder a line as `?` — and a reason is cut at 400
characters; a list of patterns or ids longer than a line is cut at a whole item
and the rest counted (`…(+N more)`). `validate` checks every file in the project's
directory, prints `ok` and `rejected` lines, a `warning` line for each pattern or
`permission` rule that names no tool, for each `allow_tools` pattern that names
only tools the `tool_classes` take away, for each `spawn` id no agent has or that
is a main agent only, and for a `when_to_use` that is never shown, and a
`summary ok=N refused=N` line, and exits 2 if any file
is refused (or a named directory is not there); in an untrusted project it
validates nothing and exits 3. `--dir <path>` names the directory to read and is
read whatever its project's trust (the files are inert text, and checking a
definition before you trust the project is what it is for). Neither command makes
the RapidLM home if it is not there. Output that scripts read changed with this
page: the `extra_dirs=` header field, the `format=` and `tools=` columns, and
reasons that were several lines are one.

## Records

A spawn resolves its type from the definitions as they are on disk when it
starts, so an edit takes effect at the next spawn. The child's `agent.spawned`
event (the `/agents` panel's source) carries two additive keys: `description`, the
label the parent gave it, and `definition_digest`, which version of its definition
it ran. A session that runs as an agent records `agent.selected`
(`rapidlm.agent.selected/v1`: `id`, `definition_digest`, `mode`) when it begins,
and again when a resume finds the definition changed; the newest is the session's
agent. Nothing else in this page writes a record.
