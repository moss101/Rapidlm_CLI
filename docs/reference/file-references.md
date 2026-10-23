# File references and completion keys

## `@path` references

Naming a file in a prompt with `@` attaches it to the turn, so the model has it without spending a tool call
to find it.

| You type | The model is given |
|---|---|
| `@src/lib.rs` | the file |
| `@src/lib.rs:40-80` or `@src/lib.rs#L40-L80` | lines 40 to 80 of it |
| `@"notes/a b.txt"` | a path with spaces |
| `@src/` | a listing of the directory |

The prompt you typed is what the ledger records; the attached text travels as separate context, in the
read-set partition at a score above retrieved snippets, so under context pressure it is the last thing
dropped. RapidLM prints a line saying what was attached and what was not.

References are honoured only in a trusted project, and only within these limits:

- The path must stay inside the project once links are resolved. `@../x`, an absolute path, and a link
  leading outside are refused. `.git` is refused.
- A file that looks like it holds a secret (the same scanner that checks files the agent writes) is not
  attached. A finding dismissed with `rapid findings dismiss` does not count.
- Binary files are refused. A text file is cut at 64 KiB at a line boundary, and the cut is stated; one
  prompt attaches at most 192 KiB and 16 references.
- `@alice` in running text, and an e-mail address, are not references. A reference to something that is
  not a file is reported only if it looks like a path (it contains `/` or `.`).

## Keys

| Key | Does |
|---|---|
| Tab | with an `@path` at the end of the prompt, completes it; with `/name`, completes the command; with an empty prompt, switches between plan and build |
| Shift-Tab | switches between plan and build, whatever the prompt holds |

Completion takes a unique match whole. With several, it extends to their shared prefix and lists them. A
name beginning with what you typed beats a looser match; when nothing begins with it, files are ranked by
how well the path fits what you typed as a subsequence, with a bonus where letters start a path segment or
word (`@kcl` finds `crates/kernel/src/client.rs`). In a git checkout, ignored files are left out. Command
completion includes your own commands (see [custom commands](custom-commands.md)).

Plan mode is the session's `/plan`: the model may read and may write only its plan.
