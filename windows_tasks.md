# windows_tasks.md — building, testing and fixing RapidLM CLI on Windows

**Written:** 2026-10-04 at `main` = `fb331c1`; **refreshed 2026-10-10 at `main` = `e38c82c`.** **Audience:** the agents
(and people) who continue this work on a Windows machine. This file is the end-to-end hand-off: the state, the
working protocol between the Windows agent and the Mac agent (§0), the rules, the setup, the open failures and the
task list. Read it in full, then read the governance it points to; do not re-derive the state it records.

**Status in one paragraph.** The SEAMS program's worklist reads 53 of 53 tasks complete
(`docs/goals/seams-implementation-tasks.json`). The latest CI run on `e38c82c`
([run 37806800669](https://github.com/moss101/Rapidlm_CLI/actions/runs/37806800669)) is **green on Ubuntu, macOS and
the SDK job, and red on Windows only: 8 failing tests** (down from 13 on 2026-10-02 — the loopback-server group, the
managed-policy test and the monitor-ordering test no longer fail). The 8 left are in §4. Windows *Lint* passes. The
Windows `Schema fixtures` step has not run since `Test` fails first, so it may hold further failures. Nothing is closed
until CI is green on all three operating systems.

---

## 0. The working protocol (Windows agent ⇄ Mac agent)

Two agents, one branch, git as the only channel.

| Who | Does | Never does |
|---|---|---|
| **Windows agent** (on the Windows 11 machine) | Reproduces, fixes, commits, pushes the branch, opens/updates the PR | Pushes to `main`; force-pushes; weakens a test to go green |
| **Mac agent** (the session that wrote this file) | Reads CI for the PR, pulls the branch, builds and tests it on macOS, reviews each commit, reports back | Edits the Windows agent's commits in place without saying so |
| **The user** | Runs `git push origin main` once the PR is green on all three OSes | — |

**Branch.** The Windows agent works on `windows/fixes` (create it from `origin/main`; if it exists, continue it). CI runs
only on pushes to `main` and on pull requests (`.github/workflows/ci.yml`), so **a branch alone gets no CI**: open a
draft PR from `windows/fixes` to `main` after the first push, then every push to the branch re-runs all three OSes.

**The loop, one slice at a time:**

1. Windows agent: pick the next task (§5), reproduce it on Windows, fix it, run the revert-cycle (rule 4), run the
   affected tests, then `cargo fmt --all -- --check` and clippy (`-D warnings`).
2. Windows agent: commit (one commit per distinct cause), update the ledger row in the same commit (rule 7),
   `git push origin windows/fixes`.
3. Windows agent: write a short **status entry** at the bottom of `windows_tasks.md` §8 (task id, commit, what was
   proven on Windows, what was not) and push it with the slice.
4. Mac agent: `git fetch`, read the PR's CI with the jobs API (§2), `git checkout windows/fixes`, build and run the
   touched tests on macOS and the full suite once, alone. Reports pass/fail per OS, with output, in §8 or the PR.
5. A red result goes back to step 1 with the failing test names. Repeat until the PR is green on all three OSes.
6. Mac agent confirms `git merge-base --is-ancestor origin/main windows/fixes` (rebase onto `origin/main` if `main`
   moved and say so), then hands the user the merge/push command in its own code block.

**Pulling work back to the Mac.** `git fetch origin && git checkout windows/fixes && git pull --ff-only`. Compile with
`cargo build --workspace --locked` and test with `cargo test --workspace --locked --no-fail-fast`, one suite at a time.
Do not commit on top of the Windows agent's branch from the Mac without telling it; use a separate branch off
`windows/fixes` and say so in the status log.

**Definition of done for the whole hand-off:** the PR's CI is green on `rust (ubuntu-latest)`, `rust (macos-latest)`,
`rust (windows-latest)` and `sdk`, including the Windows `Schema fixtures` step and the `.rapidlm` leak check; §6 has
been walked and recorded; the user has the push command.

---

## 1. Rules for every agent working here

These come from the repository's governance and from hard-won practice. They apply on Windows exactly as elsewhere.

1. **Governance.** `AGENTS.md` / `agents.md` are one-line pointers (each points at the other). The working rules are in
   `00-README.md` (the fifteen core invariants) and `docs/goals/seams-governing-principles.md` (§2 naming, §3
   invariants, §5 protocol). Read those.
2. **Naming (SEAMS §2.1).** Never name another coding-agent product in code, tests, docs, commit messages or this
   file. Upstream model providers only where §2.2 allows.
3. **Run one cargo suite at a time.** Two concurrent suites fabricate failures that vanish when re-run serially.
   Before believing a mass failure, re-run alone.
4. **Revert-cycle every fix.** A fix is trusted only after you have seen the *old* code fail the test and the *new*
   code pass it. For a timing race, run both builds many times under CPU load and compare failure counts (on
   2026-10-02 this gave 4/460 failures old vs 0/460 new). After restoring a mutated file, `touch` it — a restored file
   with an older mtime can leave cargo using the mutated artifact.
5. **Do not weaken an assertion to go green.** A test that cannot pass on Windows gets the platform difference fixed
   (or an explicit, reasoned `#[cfg]` with a Windows-side contract test), never a looser assertion.
6. **Self-review each commit.** After every feature or fix commit, run an adversarial review of the diff (a background
   reviewer agent). Every previous review found something.
7. **Record the work.** Each slice updates the delivery record / `docs/development-ledger.md` in the same commit.
   Suite-hygiene fixes go under the ledger's *Test-suite hygiene* section (`HYG-*` rows).
8. **Commits and pushing.** Commit on the local branch with the attribution trailer the session gives you. An agent's
   push to `main` may be refused by the environment; if so, do not route around it — hand the user the exact
   `git push origin main` command in its own code block. Before handing over, confirm
   `git merge-base --is-ancestor origin/main HEAD`.
9. **Tests must not touch the repository's own `.rapidlm`.** Every project a test drives lives in a temp directory
   with its own `.rapidlm` marker, and every spawned `rapid` sets `current_dir(...)` and `HOME`. CI enforces it:
   ```bash
   git status --porcelain --ignored --untracked-files=all -- .rapidlm
   ```
   must print nothing after the suite (a locally modified `.rapidlm/goal.json` counter is the one expected exception;
   do not commit it).
10. **Report faithfully.** Say what failed, with output. Say what was not run. Do not claim a Windows fix works until
    it has run on Windows.

---

## 2. Setting up the Windows machine

| Need | Detail |
|---|---|
| Rust | Toolchain **1.97.1**, pinned by `rust-toolchain.toml` (rustup installs it on first `cargo` call). Components: `rustfmt`, `clippy`. Target `x86_64-pc-windows-msvc`. |
| C toolchain | Visual Studio Build Tools with the "Desktop development with C++" workload. Several dependencies compile C (`ring`, bundled SQLite, tree-sitter grammars). |
| Git for Windows | Required. The suite resolves `sh`, `echo`, `sleep` and similar through `crates/test-fixtures` (`test_fixtures::tool_str(..)`), which looks in Git for Windows' `usr\bin`. Run commands from **Git Bash** — CI uses `shell: bash`. |
| Architecture | CI is **x86_64** Windows. Prefer an x86_64 machine; an ARM64 Windows machine builds `aarch64-pc-windows-msvc` natively and runs the `x86_64` target emulated, so timing-sensitive tests (Group A, Group C, the update-notice bound) can differ from CI. State which you used in every status entry. |
| Line endings | CI's checkout may convert line endings (see task W2). Check `git config core.autocrlf` on your machine and compare with CI before concluding anything about `\r\n` failures. |
| Node / pnpm | Only for the `sdk` job (Node 24.19.0, pnpm 11.20.0). Not needed for the Rust failures. |
| GitHub CLI | `gh`, authenticated, for reading CI. |

First commands, from the repository root in Git Bash:

```bash
git fetch origin
git switch windows/fixes 2>/dev/null || git switch -c windows/fixes origin/main
git rebase origin/main        # only while the branch has no pushed work; otherwise merge and say so
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast 2>&1 | tee windows-test.log
```

The last one is the exact CI test command. It takes 10–25 minutes. Keep the log; grep `FAILED` and `panicked at`.
Running a single test alone: `cargo test -p rapid --lib <name> -- --exact`.

Reading CI logs: `gh run view --log` truncates. Use the jobs API, which needs `--allow-escape-sequences`, and strip
colour codes:

```bash
gh run view <run-id> --json jobs --jq '.jobs[] | "\(.databaseId) \(.name) \(.conclusion)"'
gh api --allow-escape-sequences repos/moss101/Rapidlm_CLI/actions/jobs/<job-id>/logs | sed 's/\x1b\[[0-9;]*m//g' > job.log
```

---

## 3. Fixes already pushed (do not redo)

| Commit | What |
|---|---|
| `16dcae3` | `tool_call_body` in `apps/rapid/tests/configured_model_integration.rs` is `#[cfg(unix)]` — it was dead code on Windows and failed `clippy -D warnings`. Proven by simulating a non-Unix build of that file (old: `function tool_call_body is never used`; new: clean). |
| `16dcae3` | `interactive::tests::memory_consolidate_folds_summaries_into_new_revisions_and_shows_in_jobs` now waits for the job's end to be projected before asserting `Completed` (the output line and the job end are separate records). 4/460 failures before, 0/460 after, under load. |
| `fb331c1` | `a_timeout_stops_the_run_with_the_interrupted_code` (`apps/rapid/tests/agent_mode_cli.rs`) runs `rapid acp` in a project of its own. `rapid acp` opens the ledger of whatever project its working directory resolves to; on a fresh checkout that was the repository's `.rapidlm`, leaving `.rapidlm/sessions.sqlite`. |

CI on `fb331c1` confirms: Windows Lint passes, and the "Tests leave `.rapidlm` untouched" step passes on all three
operating systems.

---

## 4. The open failures (CI run 37806800669, `e38c82c`)

**8 tests fail on Windows; everything else (about 2,000 tests) passes.** Groupings and causes below are **hypotheses
from reading the panic text and the code, not confirmed** — confirm each by reproducing it on Windows before changing
code. Locations are `file:line` of the panic in that run.

### Group A — loopback-server flake: passed in the latest run, still unproven (0 failing now)

On 2026-10-02 four tests failed with `connection failed` / `rapid exec` exit 4 because a one-shot test server
(`listener.accept()` answers one connection) lost the race against a client that opens a second one. They all **passed**
in run 37806800669, which proves little for a flake. Treat as W1: run each 30 times on Windows
(`for i in $(seq 1 30); do cargo test -p <crate> --lib <name> -- --exact || break; done`); if any fails, apply the
accept-in-a-loop fix (`while let Ok((mut stream, _)) = listener.accept()`, as in `e18b0dd`) to that helper. If a server
already loops and still fails, the cause is something else (for example the client trying IPv6 for `localhost`).

| Test | Where |
|---|---|
| `providers::openai_compatible::tests::the_raw_writer_the_mcp_transport_uses_names_rapid_once` | `crates\llm-router\src\providers\openai_compatible.rs` |
| `exec_tools::tests::the_json_search_backend_goes_through_the_egress_gate_and_parses_results` | `apps\rapid\src\exec_tools.rs` |
| `empty_final_response_after_committed_work_exits_zero` | `apps\rapid\tests\exec_diagnosability.rs` |
| `json_schema_flag_prints_the_validated_result_instead_of_the_summary` | `apps\rapid\tests\exec_diagnosability.rs` |

### Group B — Windows path and line-ending assumptions in tests (4 tests failing)

| Test | Panic | Hypothesis |
|---|---|---|
| `binary_exec_worktree_runs_in_a_linked_worktree_and_leaves_the_project_untouched` and `binary_goal_worktree_is_where_the_goals_exec_turns_work` (`apps\rapid\tests\configured_model_integration.rs:1342` and `:1523`) | `left: "beta\r\n"  right: "beta\n"` | A file written by the test or checked out into the worktree comes back with `\r\n` (git `core.autocrlf`, or the file written via a Windows tool). Normalise at the point of comparison *or* make the worktree/test write bytes explicitly; first establish which side adds the `\r`. |
| `binary_du_measures_the_real_layout_and_its_plan_is_reclaims_dry_run` (same file, `:1076`) | `left: Some("C:\\Users\\...\\.rapidlm")  right: Some("\\\\?\\C:\\Users\\...\\.rapidlm")` | One side canonicalizes (Windows gives the `\\?\` verbatim form), the other does not. Decide which form `rapid du` should report and make code and test agree; do not strip the prefix in the test only. Related earlier fix: `185deb8` (verbatim and UNC ledger paths). |
| `p9_commands::release_tests::a_plugin_source_the_policy_does_not_allow_installs_nothing` (`apps\rapid\src\p9_commands.rs`) | **Passes on Windows as of `e38c82c`** (CI run 37806800669). Two causes were fixed: the test wrote its path into a TOML *basic* string (backslashes are escapes; it now uses a literal string), and `plugin_source` judged `canonicalize`'s verbatim `\\?\C:\...` form so an administrator's `C:\approved\*` rule could never match (it now judges the plain form, `plain_path_text`). **Still open for W2:** rules written with `/` instead of `\` are not normalised — decide whether they should be, and add the product-level test. |
| `binary_exec_worktree_subagents_work_in_the_worktree_not_the_project` (same file, `:1445`) | assertion failure with the run's stderr (the child patch merged into the *parent* workspace: "Changes were applied to the parent workspace (three-way merge)") | Needs reading the full assertion on Windows; possibly the same path-form difference as `du`, possibly a real defect in how the worktree root is compared. |

### Group C — monitor and timing behaviour (4 tests failing; the macOS one is fixed)

These test the new `monitor` tool (SEAM-03-4). They may expose **product** differences, not just test differences.

| Test | Panic | Hypothesis |
|---|---|---|
| `exec_tools::tests::a_flooding_monitor_is_stopped_once_and_says_so` (`apps\rapid\src\exec_tools.rs:12438`) | the final record is not `end cancelled` | The monitored command is `sh -c 'while ...; sleep 30'`. On Windows, stopping it may report `end completed` — the process-tree termination or the exit-status mapping differs. |
| `exec_tools::tests::a_monitor_ends_with_its_turn_unless_persistent` (`:12475`) | `the persistent monitor still runs` — `left: 2, right: 1`. (On 2026-10-02 the message was `never saw end cancelled: [... "end completed", "end completed"]`: both monitors end `completed`.) | Same: a monitor that should be cancelled at turn end is recorded `completed`. |
| `interactive::tests::a_monitors_lines_arrive_as_notices_and_a_turns_monitor_ends_with_it` (`apps\rapid\src\interactive.rs:25417`) | `the persistent monitor outlives its turn` | Same family. |
| `update_notice::tests::a_closed_port_and_a_silent_server_cost_at_most_the_timeout` (`apps\rapid\src\update_notice.rs:954`) | elapsed `2.0162493s` against a `< 2s` bound | Windows retries a TCP connect to a closed loopback port for about 2 s before reporting refusal. The product's bound is its configured timeout (3 s in the test); the *test's* 2 s bound encodes a Unix expectation. Decide whether the product should cap connect time lower on Windows, or the test should assert against the configured timeout. |
| ~~macOS: `exec_tools::tests::every_line_is_recorded_before_the_monitors_end`~~ | **Fixed in `d781436` — not a product race.** The monitor's end waits at most `MONITOR_OUTPUT_SETTLE` (2 s) for its reader and nothing is recorded after the end, by design (`no_line_is_recorded_after_the_monitors_end`). The test made its sink sleep 20 ms on each of 30 lines, so on a loaded runner the sleeps summed past the settle (27 of 30 recorded). With 80 ms a line the old test fails the same way locally. It now holds one line for twice a plain job's settle. The same shape — many short sleeps that must sum to under a fixed budget — is worth looking for if a Windows monitor test is slow rather than wrong. |

Unknowns to close: the Windows job runs `Schema fixtures` after `Test`, and it was skipped because `Test` failed — it
may hold further failures. Check after the list above is green.

---

## 5. Tasks for the Windows machine

Do these in order. After each task: format, clippy (`--workspace --all-targets -D warnings`), the affected tests,
revert-cycle, commit, self-review. Run the full suite once at the end of each group, alone.

- **W0 — Reproduce.** Run the full CI test command (§2) on Windows. Confirm the 8 failures above (Group B 4, Group C 4) are the
  same 8 (or record the difference, including any Group A test that fails). Save the log. Check `git status --porcelain --ignored --untracked-files=all -- .rapidlm`
  afterwards.
- **W1 — Group A (loopback servers).** Run the four tests 30 times each; fix any helper that still fails; prove each by running the test repeatedly
  (`for i in $(seq 1 30); do cargo test -p <crate> --lib <name> -- --exact || break; done`), old vs new.
- **W2 — Group B (paths and line endings).** One commit per distinct cause: the `\r\n` cause (find where the `\r`
  enters), the `\\?\` form in `rapid du`, the TOML-escaped policy path. For the policy test also add a product-level
  test that a managed `allowed_sources` rule matches a Windows-style path, if it currently cannot.
- **W3 — Group C (monitor and timing).** Determine why a cancelled monitor is recorded `completed` on Windows (process
  group / job-object termination and exit-status mapping in the job supervisor; compare with how `rapid` stops
  ordinary background jobs on Windows). Fix the product if it is a product defect; add a Windows-side contract test.
  Resolve the update-notice bound as described. The macOS ordering failure is already fixed (`d781436`).
- **W4 — Whole-suite pass.** Full `cargo test --workspace --locked --no-fail-fast` green on Windows, then the
  `cargo test --locked -p protocol --test schema_fixtures` step, then the leak check.
- **W5 — Push and read CI.** Hand the user the push command; read the run for all three operating systems with the jobs
  API; fix whatever is left. Do not poll CI in a loop; read it when the user says it finished.

---

## 6. Revisit once Windows work is developed

When W0–W5 are done and CI is green on Ubuntu, macOS and Windows, come back to the build as a whole:

1. **A clean-checkout run.** Confirm CI on a fresh commit passes all jobs, including the leak check and `Schema fixtures`.
2. **Windows binary smoke.** `scripts/release-smoke.sh` and `scripts/release-sign.sh` are bash scripts; check they run
   under Git Bash on Windows and that `rapid --version`, `rapid doctor`, `rapid setup --dry-run`, a headless
   `rapid exec` against a scripted model, and `rapid acp` start work. Record what does not.
3. **Windows-specific product surface.** Walk the features that touch the OS and record a typed result for each:
   sandboxed execution and PTY tiers (reported as typed unavailability on Windows — confirm the message names the
   next step), worktrees (`rapid worktree list|reclaim`, `rapid du`), background jobs and the process-tree stop,
   the status-line command, keychain/credential storage, the update notice, and `rapid mcp install` on Windows config
   locations.
4. **Docs.** `docs/getting-started.md` and the CLI reference should state what is and is not supported on Windows,
   including the Git for Windows requirement for the test suite.
5. **Older open items** still listed in `docs/gap-to-delivery.md`: `rapid run` background-process steps, a standalone
   `rapid sandbox` command, release signing and update (waiting on a credential), line-level three-way merges
   (deliberately partial), and signing/verification of authenticity (partial). Decide with the user whether any of
   them blocks calling the CLI complete.
6. **Update the records.** Mark the work in `docs/development-ledger.md`, and make the final statement in the delivery
   record only after step 1 is true.

---

## 7. Where things are

| What | Where |
|---|---|
| CI definition | `.github/workflows/ci.yml` (jobs: `rust` on ubuntu/macos/windows, `sdk`) |
| The SEAMS worklist (authority on task status) | `docs/goals/seams-implementation-tasks.json` |
| Delivery record per slice | `docs/goal-delivery-2026-09-20.md` |
| Ledger of work and hygiene rows | `docs/development-ledger.md` |
| Older gap list | `docs/gap-to-delivery.md` |
| Windows test fixtures (shell tools) | `crates/test-fixtures` |
| Monitor tool and its tests | `apps/rapid/src/exec_tools.rs` (tests around lines 12000–12420) |
| Update notice and its tests | `apps/rapid/src/update_notice.rs` |
| Managed policy | `apps/rapid/src/managed_config.rs`, `crates/security` |
| Provider transport and its tests | `crates/llm-router/src/providers/openai_compatible.rs` |

---

## 8. Status log (append-only; newest last)

Each entry: date, agent (Windows/Mac), machine (OS, x86_64 or ARM64), task id, commit, what was proven (with the
command and the count), what was not run. A failing result goes here too, with its output.

- **2026-10-10 (Mac):** hand-off refreshed against CI run 37806800669 on `e38c82c`: Windows red (8 tests), Ubuntu and
  macOS green. No Windows work done yet. A Windows 11 ARM64 VM exists on the Mac (Parallels) but has no toolchain;
  the intended machine is a physical Windows 11 device.
