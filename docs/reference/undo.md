# Undo and redo

`/undo` takes back the last turn: the files it changed, the conversation it added, and your prompt, which
returns to the composer unsent. `/redo` brings the turn back.

| Command | Does |
|---|---|
| `/undo` | restore the files the last turn changed, rewind the conversation to just before it, put the prompt back in the composer |
| `/undo --preview` | list what would change; change nothing |
| `/undo --force` | undo even over files that have changed since the turn |
| `/redo` | reapply the undone turn's files and return to the session where the turn exists |
| `/redo --force` | the same, over files that have changed since the undo |

Run `/undo` again to go back another turn. A turn that is running cannot be undone; interrupt it first.

## What is restored

Before each turn and after it, RapidLM records the project's working tree in a private store under
`.rapidlm/undo/`. The store is a bare git repository holding tree objects only, with the project as its
work tree, so a change made by a shell command is captured the same as one made by a file tool.

- The project's own ignore rules apply. Build output, dependencies and anything in `.gitignore` are not
  recorded, so an ignored file the agent edited is not restored.
- Inside `.rapidlm/`, only `commands/` and `MEMORY.md` are recorded. The session database, locks and the
  store itself are not.
- Line-ending conversion and content filters (such as Git LFS) are switched off for the store, so what
  comes back is the bytes that were on disk.
- Symbolic links and nested repositories are reported and left alone.
- Executable bits are restored on platforms that have them.

## Nothing is overwritten by accident

A file is moved only if it is still as the turn left it. If you, a formatter or a later command changed it,
that is a conflict, and `/undo` stops before changing anything:

```
/undo stopped: files changed since that turn would be overwritten
  revert src/lib.rs
  CONFLICT src/main.rs (changed since)
/undo --force overwrites them; /undo --preview shows the plan
```

A file you have already put back by hand is not a conflict. `--force` overwrites conflicting files and still
refuses paths that would leave the project.

## The conversation

The ledger is append-only, so `/undo` does not delete the turn. It forks the session at the point just
before the turn and switches to the fork, the same move as `/rewind`. The original session keeps the turn
in full, and `/redo` returns to it. Sending a new prompt after an undo ends the chance to redo.

## Limits and housekeeping

- The last 50 turns of a session are kept. Journals untouched for 14 days are removed, and the recorded
  trees no journal names are collected the next time a session starts.
- The first turn in a project records the whole tracked tree, so it can take a moment in a very large
  repository. Set `RAPIDLM_NO_UNDO=1` to turn recording off.
- A project without a working `git` on `PATH` cannot record. `/undo` says so once and the turn runs
  normally.
- Files changed by background jobs or subagents after a turn ends are not part of that turn.
