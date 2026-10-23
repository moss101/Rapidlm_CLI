# Custom commands

A Markdown file is a reusable prompt. Put it in a commands directory and run it as a slash command.

| Where | Available in | Loaded when |
|---|---|---|
| `<project>/.rapidlm/commands/` | that project | the project is trusted (`rapid trust grant`) |
| `<rapidlm home>/commands/` | every project | always |

A project file wins over a user file of the same name. A file in a subdirectory takes the directory as a
prefix: `git/pr.md` is `/git:pr`. Names are lowercase letters, digits, `-` and `_`. `/commands` lists
what was found, and any file that was passed over with the reason.

## File format

```markdown
---
description: Review a file for error handling
argument-hint: <file> [focus]
---
Review $1 with attention to $2. Report problems only; do not edit files.
```

The front matter is optional. `description` and `argument-hint` are shown by `/commands`; other keys
are ignored.

| In the body | Becomes |
|---|---|
| `$ARGUMENTS` | everything typed after the command name |
| `$1` … `$9` | the arguments, split like a shell does (quotes group, a backslash escapes) |
| `$$` | a literal `$` |

A missing `$n` is empty. A body that uses none of these gets the arguments appended after a blank line.

## What a command cannot do

- It expands to text submitted as an ordinary prompt. It meets the same prompt hooks, permission checks
  and approvals as anything typed.
- There is no shell substitution or file inclusion in a template.
- A built-in command is never replaced. A file named `help.md` is reported by `/commands`, not run.
- Symbolic links are not followed, files over 64 KiB are skipped, and nesting stops at three levels.
- An untrusted project's commands are not loaded, because they are text the model would follow.
