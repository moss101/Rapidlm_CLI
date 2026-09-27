---
name: rapid-headless
description: Drive the `rapid` coding agent headlessly from another program — run one agent turn, read its result and exit code, continue a session, and hand it keys safely. Use when a task should be delegated to `rapid` from a script, CI job or another agent host.
---

# Driving `rapid` headlessly

`rapid` is a coding agent. With no subcommand it opens an interactive TUI; everything
below is the headless surface, meant for scripts, CI and other agent hosts. Every
command line in this package is checked against the binary's own `--help` by a test,
so a flag named here exists.

## Before the first run

A project must be trusted once before any workspace tool (file edits, shell) runs.
Trust is only ever granted explicitly, by a person, in the project directory:

```bash
rapid trust status
rapid trust grant
```

A model must be configured. `rapid setup` writes the configuration after verifying it
with one real request; `rapid doctor` diagnoses what is missing without contacting a
provider:

```bash
rapid setup --preset <id> --key-env <VAR>
rapid doctor
```

## One headless turn

```bash
rapid exec "fix the failing test in src/parser.rs"
```

- **stdout** carries only the result: the final response, or with `--json-schema` the
  schema-conformant JSON, or with `--jsonl` the protocol records.
- **stderr** carries diagnostics (`--verbose` adds per-attempt detail). Never parse it.
- The **exit code** says why the run ended — branch on it (table below).

Useful flags:

```bash
rapid exec "summarise the change" --jsonl
rapid exec "extract the version" --json-schema schema.json
rapid exec "long task" --max-wall-time 600 --usage-file usage.json
rapid exec "plan the refactor" --plan
```

- `--jsonl` writes JSONL protocol records (a schema record, `assistant.message`,
  `session.finished`) instead of plain text. Read line by line; each line is one JSON
  object.
- `--json-schema <path>` makes the result a JSON document matching that schema, or
  the run fails — it never prints a non-conforming result.
- `--usage-file <path>` writes tokens, cost and tool calls as JSON; stdout is unchanged.
- `--plan` lets the turn write only its plan; the proposal is printed and the run
  exits `10` for a person to approve.
- `--max-wall-time <secs>` cancels a turn that runs too long, exactly as Ctrl-C would.

Per-run environment: `RAPIDLM_MODEL` (model id), `RAPIDLM_CONFIG` (config file),
`RAPIDLM_PERMISSION_MODE` (`default`, `plan`, `acceptEdits`, `auto`, `dontAsk`,
`bypassPermissions`).

## Exit codes

<!-- exit-codes: must equal docs/getting-started.md; checked by skill_package -->

| Code | Meaning |
|---|---|
| `0` | the run produced its final answer |
| `2` | bad arguments, missing or invalid configuration, unknown session |
| `3` | a policy or permission decision stopped the run |
| `4` | the model provider failed or refused |
| `5` | the agent turn failed for a reason other than the provider |
| `6` | the run ended with its goal's completion criteria unmet |
| `7` | the sandbox refused or could not run a command |
| `8` | a budget (tokens, cost, time, or turns) ran out |
| `9` | the model stopped to ask for something only you can supply; re-run with it |
| `10` | paused for a human decision: a workflow run (`rapid run --resume <id>` continues it) or an exec turn a hook asked about (`rapid resume <session>`, then /approvals) |
| `130` | interrupted (Ctrl-C or an external cancel) |

`9` is neither success nor a failure to retry: answer what was asked and run again.
`10` needs a person; never approve on their behalf.

## Resuming

Every run is recorded as a session. Continue one — so the model sees what was said
before — by its id, or the most recent:

```bash
rapid exec "now add the test" --resume <session-id>
rapid exec "and update the changelog" --continue
rapid sessions list
```

Rules:

- **Wait on the exact session.** Resume the id the earlier run reported, not "the
  latest", when more than one caller may be running.
- **Never start a second goal because a resume was interrupted.** An interrupted run
  (`130`) left its session and its goal intact; resume that session.
- A resume of an id the project never recorded exits `2` and lists the ids it has.

## Keys

- A key is never passed on the command line and never written into a prompt.
- Hand `rapid setup` the *name* of an environment variable that holds it
  (`--key-env <VAR>`), or pipe it on stdin (`--key-stdin`), which stores it in the OS
  keychain.
- Never print, log or commit a key; `rapid` itself redacts registered secrets from
  tool output.

## Long-running work

For work that spans many turns — a goal with completion criteria and evidence — see
[`long-running-goals/SKILL.md`](long-running-goals/SKILL.md).
