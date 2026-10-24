# ADR 0026 — Agent definitions in Markdown, and the narrowing a definition can ask for

**Status:** Accepted for `EXT-05` (slices 1–3 built; slices 4–5 add to the Consequences below)  
**Date:** 2026-10-24  
**Builds on:** ADR 0023 §5 (an agent type resolves through the definitions; a definition is an overlay), the permission lattice, the managed policy; invariants 1, 3, 12, 13; `SEAMS §3.2 S1, S3, S5, S7, S9, S11`

## Context

- A definition was a TOML table in a coarse vocabulary: ten tool *classes*, no tool names, no step cap. A project could not say "read-only, these tools only, twelve steps".
- A skill is a Markdown file whose front matter a loader reads with a closed subset of YAML. Two readers of the same shape (skills, definitions) would drift unless they share one.
- The risk of a more expressive definition is that it becomes a way to widen: a tool the role does not have, a rule that approves what the user would be asked about, a prompt that replaces the base one.

## Decision

1. **Two syntaxes, one definition.** `<id>.md` (front matter, then the instructions as the body) and `<id>.toml` are turned into the same table and read by one validator. Equivalence is therefore a property of the code, and a property test over generated definitions (refused ones included) keeps it so. The Markdown form has no `instructions` key (the body is the instructions), no way to template over the base prompt, and spells the table form's `tools` as `tool_classes`.
2. **One front matter reader, in `protocol`.** The closed subset the skills loader read moved to `protocol::frontmatter`; the loader reads through it, the old reader stays in the skills tests as an oracle compared against on generated input (cancellation, an expired time and the scalar bound included). Each consumer keeps its own closed key set: the skills privilege-key denylist stays with skills, and the one construct skills do not have (a one-level map whose keys may be quoted, for `permission`) is switched on by the agent schema only.
3. **A definition can only narrow, and says so in its syntax.** The surface is `(base role ∩ tool_classes) ∩ allow_tools − deny_tools`; `allow_tools` names tools the classes already allowed or nothing, `permission` takes `deny` and `ask` and refuses `allow` at load, `max_steps` is 1–32. Enforcement is at the two points the class narrowing always had — the tool is not offered, and a call to it is refused as `tool_not_allowed_for_agent` — and the rules go on the child's lattice after its write scope, where the managed policy's bans still come first and `deny` outranks `ask`. A v1 table keeps its closed key set; the new keys need `schema = "rapidlm.agent_defs.v2"`.
4. **A step cap ends a child with a report, not an error.** The cap is a field of the child's budget; reaching it stops the child with the existing stop reason `budget_exhausted` and the detail `steps`, and the parent is handed what the child did (its last calls and their outcomes) with the status `budget_exhausted`. Such a child is *unfinished*: headless its work is dropped, interactive it is held for review, like a child that needed context. (A spent token budget still fails the child; that is not changed here.)
5. **A project's definitions are read where the project is.** A trusted project's directory is read only if, with every link on the way followed, it is inside the project; a symlinked definitions directory is not followed anywhere (`[agents] extra_dirs` names the target for a user who links theirs); `rapid agents list|validate` do not read an untrusted project (`--dir` names a directory to read, whatever its trust). Everything a definition file says is shown on one clean, bounded line.
6. **Checked beside, not instead.** The grammar of a `permission` key is the host's (it is the project settings' rule grammar), so the loader asks the host through the implementation registry; a pattern that names no tool is a warning in `rapid agents validate` with the closest names, because an MCP tool may not be connected yet.

## What is deliberately not done

- No new stop-reason token (touching the hooks reference, the SDK and every consumer of stop reasons for a detail the existing one carries).
- No `allow` in an agent file, and no way to raise a role's tools by a definition.
- No second matcher: patterns use the permission lattice's `*`/`?` glob, the one `[mcp]` tool filters use.
- Nested spawning stays at depth 1 (a later decision, with its own spec).

7. **Steering is data, bounded like the rest.** `mode` says where an agent may be used; `when_to_use` rides on its entry in the list `task_spawn` describes (the existing 2 KiB bound and its "N more" summary still hold, and a configuration without the keys gets the same text); `spawn` is an allowlist for a main agent, refused on a subagent definition at load, enforced where the tool offers types and where it accepts one, and not applied to continuing a child. The spawn record gains the label the parent gave and a digest of the definition the child runs.

## Consequences

- `docs/reference/agent-definitions.md` is the user-facing page; `rapid agents list` shows format, source, classes, patterns, rule count and step cap; the `/agents` panel shows the same in a line.
- The wire form of a budget is unchanged without a cap (`max_steps` is written only when set), so no golden moved.
- Slices 4–5 (a selectable main agent, MCP for children) extend this ADR.
