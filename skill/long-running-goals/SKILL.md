---
name: rapid-long-running-goals
description: Run a multi-turn `rapid` goal to verified completion from another program — create the goal with criteria, drive turns, claim completion with checks, and resume safely. Use when delegated work needs more than one `rapid exec` turn and must end with evidence, not a claim.
---

# Long-running goals with `rapid`

A goal is the project's durable objective: a statement, completion criteria and a
budget. Every `rapid exec` turn in the project accrues to the active goal, and the goal
is complete only when its criteria are backed by evidence — never because a turn said
so. There is one active goal per project.

## Create it

```bash
rapid goal create "parser handles nested quotes" --criterion "tests=the parser suite passes" --requires "tests=test" --max-steps 40
```

- `--criterion <id>=<text>` (repeatable): what must be true at the end.
- `--requires <id>=<kind>[,<kind>]` (repeatable): the evidence kinds criterion `<id>`
  needs — `test`, `build`, `lint`, `command` and others; a claim's check records them.
- `--max-steps <n>`, `--max-tokens <n>`: the budget; running out exits `8`.

A second `create` while a goal is active is refused. `replace` swaps the active goal:

```bash
rapid goal replace "parser handles nested and escaped quotes" --criterion "tests=the parser suite passes" --requires "tests=test"
```

## Drive it

Run turns with `rapid exec` (see the parent skill), continuing the same session:

```bash
rapid exec "make the parser suite pass" --continue
rapid goal show
rapid goal verify
```

`show` prints the goal, its criteria and whether it can complete; `verify` prints each
criterion's verdict and whether a retry could change it.

## Finish it with evidence

Claim completion by naming, per criterion id, the command that proves it. `rapid`
runs each command itself, records the result as evidence of the kinds `--requires`
named, and accepts only on fresh passing checks. Quote each `--check` value — the
command is one argument:

```bash
rapid goal claim --summary "nested quotes parse" --check "tests=cargo test -p parser" --timeout-secs 300
```

Exit `0` means accepted; `6` means the criteria are still unmet — read the per-check
lines on stdout and continue working, do not re-claim unchanged.

Other lifecycle commands:

```bash
rapid goal pause
rapid goal resume
rapid goal cancel
rapid goal export
```

## Rules

- **One goal, one session line.** Resume the session that was working the goal; an
  interrupted turn (`130`) left both intact.
- **Never start a second goal because a resume was interrupted**, and never
  `replace` a goal to escape its budget or criteria.
- **Evidence, not assertions.** A turn saying "done" completes nothing; `claim` with
  real checks does.
- A human decision (`10`) is a person's to make.
