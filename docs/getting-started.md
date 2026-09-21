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
| Windows, x86_64 | `rapid-x86_64-pc-windows-msvc.zip` — built, linted and tested on every push like the other two; the POSIX-only tiers report themselves unavailable there (see below) |

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

**Windows.** The archive is a real build of the same code, and CI's Windows job runs the full
test suite on every push as a gate (since 2026-09-17), the same as macOS and Linux. What differs
there is stated rather than emulated: hooks run under `cmd /C` instead of `sh -c`; a cancelled
`shell_exec` background job is terminated as a process, not a tree (a `cmd /C build.bat` job's own
children can outlive it), while supervised children — external agents, evaluation runs and the
sandbox backends — are stopped as trees through `taskkill`; the PTY session and the
host-restricted sandbox tier need POSIX tools (`script(1)`, `ulimit`, `ps`) and report themselves
unavailable, so `shell_exec` with `sandbox: true` and configured external scanners fail closed
rather than run unconfined; and the daemon and its socket are Unix-only. Git for Windows is
required for the workspace backend, and its `usr/bin` tools are what the test suite drives.

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
| `10` | paused for a human decision: a workflow run (`rapid run --resume <id>` continues it) or an exec turn a hook asked about (`rapid resume <session>`, then /approvals) |
| `130` | interrupted (Ctrl-C or an external cancel) |

## Sandbox levels, truthfully

`shell_exec` with `"sandbox": true` reports the protection you actually got,
by name:

- **macOS with `sandbox-exec`**: the Seatbelt tier — filesystem writes and
  network are genuinely confined (`[sandbox: seatbelt — filesystem writes and
  network are confined]`).
- **Everywhere else** (Linux, Windows, macOS without `sandbox-exec`): the
  host-restricted tier — real process-group isolation plus CPU/memory/pid
  limits, but filesystem and network are **not** confined
  (`[sandbox: host-restricted — resource limits only; filesystem and network
  are NOT confined]`).

Set `RAPIDLM_SANDBOX_REQUIRED=1` to fail those calls closed instead of
degrading: on a platform that cannot confine, the call is refused with the
reason rather than running unprotected. `rapid doctor` probes the same
backends and reports which tiers are available on this machine.

## A real browser, one command

`rapid browser <url>` launches the Chromium-family browser already on your machine (Google
Chrome, Chromium or Microsoft Edge; `RAPIDLM_BROWSER_PATH` overrides discovery) headless, opens
an isolated context, navigates, runs `--step` actions through the same observe/act/verify path
the agent runtime uses, and prints what the page looks like to the model — URL, title and the
semantic targets (locators only; field values are never read). Screenshots and the session
trace are stored as artifacts under `.rapidlm/browser/`.

```sh
rapid browser https://example.test/ \
  --step 'type:email=me@example.test' \
  --step 'click:sign-in' \
  --step 'expect-url:https://example.test/home' \
  --screenshot
```

`rapid browser --help` lists every step kind. Only Chromium engines are live; a page with a
password field gets a redacted screenshot, not pixels.

## Where to next

- [`docs/reference/cli-command-reference.md`](reference/cli-command-reference.md) —
  every subcommand, with the paragraph that says which ones ship checked against the
  dispatcher.
- [`docs/reference/model-configuration.md`](reference/model-configuration.md) — the
  configuration schema in full.
- `rapid <subcommand> --help` — each command documents itself.
