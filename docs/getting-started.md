# Getting started

The shortest honest path from nothing to a working `rapid`. Every command here was
run as written; the sections that describe the binary's behavior are checked
against its source by tests named beside them.

## Install

**Pre-built binaries.** Each `v*` tag publishes a GitHub Release with one archive per
supported target and a SHA-256 beside it (`.github/workflows/release-matrix.yml`):

| Target | Archive |
|---|---|
| macOS, Apple silicon | `rapid-aarch64-apple-darwin.tar.gz` |
| Linux, x86_64 | `rapid-x86_64-unknown-linux-gnu.tar.gz` |
| Windows, x86_64 | `rapid-x86_64-pc-windows-msvc.zip` — builds and lints on every push; the test suite is not yet run there, and job control, hooks and sandboxing are Unix-first (see below) |

```sh
tar -xzf rapid-aarch64-apple-darwin.tar.gz     # or unzip the Windows archive
shasum -a 256 -c rapid-aarch64-apple-darwin.tar.gz.sha256
mv rapid ~/.local/bin/                          # anywhere on your PATH
```

**From source.** The repository pins its toolchain in `rust-toolchain.toml`; `rustup`
installs it on first use.

```sh
cargo install --path apps/rapid --locked
```

`--locked` builds from exactly the lockfile that was tested. Nothing is fetched at
runtime and nothing is written outside the directories named below.

**Windows.** The archive is a real build of the same code, checked by CI's Windows job for
compiling and linting clean on every push — but the test suite is exercised on macOS and Linux
only, and the parts of RapidLM that drive processes are Unix-first: background jobs are signalled
by process group, hooks run under `cmd /C` instead of `sh -c`, the PTY and sandbox tiers need
Unix tools, and the daemon and its socket are Unix-only. Expect the model, transcript, files and
settings to work and the process-heavy features to be incomplete until Windows gets its own test
pass.

## First run

`rapid` keeps two kinds of state: **user** configuration under `~/.rapidlm/` and
**project** state under `.rapidlm/` in the project root — the nearest ancestor of the
current directory with a `.rapidlm` or `.git`. Start from inside a project.

**1. See what is missing.**

```sh
rapid doctor
```

Doctor exits `0` when nothing failed and `1` when any check did; on a fresh machine it
reports the model as unconfigured and exits `1`. That is the command working, not
breaking: its job is to tell you what to do next.

**2. Configure a model.** A small TOML selects the provider, model and credential.
The full schema, precedence and every provider are in
[`docs/reference/model-configuration.md`](reference/model-configuration.md); this is
the local-Ollama shape:

```toml
# ~/.rapidlm/config.toml
[models]
default = "ollama-local"

[model.ollama-local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"
env_key = "OLLAMA_API_KEY"
```

`RAPIDLM_CONFIG=<path>` points at a different file; `RAPIDLM_MODEL=<profile>`
overrides the default for one invocation.

**3. Trust the project.** Until you do, the agent can read your prompt but cannot
touch the workspace: file and shell tools, proactive context retrieval, hooks and MCP
servers are all gated on trust, and `rapid exec` says so at the top of its output.

```sh
rapid trust grant       # for the project the current directory is in
rapid trust status      # `revoke` undoes it
```

Trust is decided for the project the command actually runs in — never for a path you
name — so the thing being trusted is always the thing that will run.

**4. Run something.**

```sh
rapid exec "explain what this repository does"   # one headless turn, output to stdout
rapid                                             # the interactive TUI
```

Inside the TUI, `/help` lists every slash command and marks which ones work in this
build. `/models`, `/context`, `/jobs` and `/diff` show what the session resolved,
compiled, is running and has changed; `/resume` reopens an earlier session of this
project. Every turn carries the session's earlier turns; when they grow long,
`/compact` has the model fold them into a summary that later turns — and
`rapid exec --continue` — read in their place.

## Exit codes

`rapid exec` and the other headless commands exit with a specific code so a script can
branch on *why* a run ended, not just whether it did. `9` matters most: the model
correctly stopped to ask for something only you can supply, which is neither a success
nor a failure to retry — re-run with the missing input.

<!-- exit-codes: checked against `JsonlExitCode::ALL` by `getting_started_lists_exactly_the_exit_codes` -->

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
| `10` | a workflow run paused for a human decision (`rapid run --resume`) |
| `130` | interrupted (Ctrl-C or an external cancel) |

## Where to next

- [`docs/reference/cli-command-reference.md`](reference/cli-command-reference.md) —
  every subcommand, with the paragraph that says which ones ship checked against the
  dispatcher.
- [`docs/reference/model-configuration.md`](reference/model-configuration.md) — the
  configuration schema in full.
- `rapid <subcommand> --help` — each command documents itself.
