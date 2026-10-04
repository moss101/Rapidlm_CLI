# windows_tasks.md — building, testing and fixing RapidLM CLI on Windows

**Written:** 2026-10-04, at `main` = `fb331c1`. **Audience:** the agents (and people) who continue this work on a
Windows machine. This file is both the hand-off for the CI-red state found on 2026-10-02 and the task list for
Windows. Read it in full, then read the governance it points to; do not re-derive the state it records.

**Status in one paragraph.** The SEAMS program's worklist reads 53 of 53 tasks complete
(`docs/goals/seams-implementation-tasks.json`), and Ubuntu CI is green. The latest CI run on `fb331c1`
([run 36999423899](https://github.com/moss101/Rapidlm_CLI/actions/runs/36999423899)) is **red on Windows (13 failing
tests) and macOS (1 failing test)**. Windows tests had not run since 2026-09-28, because the Windows *Lint* step was
failing and everything after it was skipped. Lint is fixed now, so a week of Windows-only breakage surfaced at once.
Nothing is closed until CI is green on all three operating systems.

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
| Line endings | CI's checkout may convert line endings (see task W2). Check `git config core.autocrlf` on your machine and compare with CI before concluding anything about `\r\n` failures. |
| Node / pnpm | Only for the `sdk` job (Node 24.19.0, pnpm 11.20.0). Not needed for the Rust failures. |
| GitHub CLI | `gh`, authenticated, for reading CI. |

First commands, from the repository root in Git Bash:

```bash
git pull --ff-only origin main
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

## 4. The open failures (CI run 36999423899)

Groupings and causes below are **hypotheses from reading the panic text and the code, not confirmed** — confirm each
by reproducing it on Windows before changing code. Locations are `file:line` of the panic in that run.

### Group A — probably the known loopback-server flake (4 tests)

A one-shot test server (`listener.accept()` answers a single connection) loses the race against a client that opens a
second connection (preflight or retry). Symptom: `connection failed` / `rapid exec` exits 4. The earlier fix
(`e18b0dd`) was to **accept in a loop** — `while let Ok((mut stream, _)) = listener.accept()` — answering every
connection with the scripted body; that matches a real server. Apply the same to each helper.

| Test | Panic |
|---|---|
| `providers::openai_compatible::tests::the_raw_writer_the_mcp_transport_uses_names_rapid_once` (`crates\llm-router\src\providers\openai_compatible.rs:4540`) | `exchange: Connection` |
| `exec_tools::tests::the_json_search_backend_goes_through_the_egress_gate_and_parses_results` (`apps\rapid\src\exec_tools.rs:21891`) | `results: "the search backend failed: provider connection failed"` |
| `empty_final_response_after_committed_work_exits_zero` (`apps\rapid\tests\exec_diagnosability.rs`) | log shows `model step ... attempt=0 outcome=failed:connection`, then `empty_response`; expected exit 0 |
| `json_schema_flag_prints_the_validated_result_instead_of_the_summary` (`apps\rapid\tests\exec_diagnosability.rs`) | `agent turn failed: model_failed (connection ...)`; `left: Some(4) right: Some(0)` |

Caveat: if a server already loops and the failure persists, the cause is something else (for example the client
connecting to `localhost` and trying IPv6 first) — investigate before assuming the flake.

### Group B — Windows path and line-ending assumptions in tests (4 tests)

| Test | Panic | Hypothesis |
|---|---|---|
| `binary_exec_worktree_runs_in_a_linked_worktree_and_leaves_the_project_untouched` and `binary_goal_worktree_is_where_the_goals_exec_turns_work` (`apps\rapid\tests\configured_model_integration.rs`) | `left: "beta\r\n"  right: "beta\n"` | A file written by the test or checked out into the worktree comes back with `\r\n` (git `core.autocrlf`, or the file written via a Windows tool). Normalise at the point of comparison *or* make the worktree/test write bytes explicitly; first establish which side adds the `\r`. |
| `binary_du_measures_the_real_layout_and_its_plan_is_reclaims_dry_run` (same file) | `left: Some("C:\\Users\\...\\.rapidlm")  right: Some("\\\\?\\C:\\Users\\...\\.rapidlm")` | One side canonicalizes (Windows gives the `\\?\` verbatim form), the other does not. Decide which form `rapid du` should report and make code and test agree; do not strip the prefix in the test only. Related earlier fix: `185deb8` (verbatim and UNC ledger paths). |
| `p9_commands::release_tests::a_plugin_source_the_policy_does_not_allow_installs_nothing` (`apps\rapid\src\p9_commands.rs:4522`) | `policy: Parse { reason: "at line 4, column 26" }` | The test builds a TOML policy with `format!("... allowed_sources = [\"{}/*\"]", canonicalize(..).display())`. On Windows that path contains backslashes (and `\\?\`), which are escape sequences in a TOML basic string. Use a TOML literal string (single quotes) or escape the path. Then check the product question too: does `plugins.allowed_sources` matching (SEAM-06-2, `crates/security` / `managed_config.rs`) handle Windows separators and `\\?\` prefixes? A policy that silently fails to match on Windows is a security bug, not a test bug. |
| `binary_exec_worktree_subagents_work_in_the_worktree_not_the_project` (same file) | assertion failure with the run's stderr (the child patch merged into the *parent* workspace: "Changes were applied to the parent workspace (three-way merge)") | Needs reading the full assertion on Windows; possibly the same path-form difference as `du`, possibly a real defect in how the worktree root is compared. |

### Group C — monitor and timing behaviour (4 on Windows, 1 on macOS)

These test the new `monitor` tool (SEAM-03-4). They may expose **product** differences, not just test differences.

| Test | Panic | Hypothesis |
|---|---|---|
| `exec_tools::tests::a_flooding_monitor_is_stopped_once_and_says_so` (`apps\rapid\src\exec_tools.rs:12414`) | the final record is not `end cancelled` | The monitored command is `sh -c 'while ...; sleep 30'`. On Windows, stopping it may report `end completed` — the process-tree termination or the exit-status mapping differs. |
| `exec_tools::tests::a_monitor_ends_with_its_turn_unless_persistent` (`:12017`) | `never saw end cancelled: ["job-1: turn", "job-2: session", "end completed", "end completed"]` | Same: a monitor that should be cancelled at turn end is recorded `completed`. |
| `interactive::tests::a_monitors_lines_arrive_as_notices_and_a_turns_monitor_ends_with_it` (`apps\rapid\src\interactive.rs:25417`) | `the persistent monitor outlives its turn` | Same family. |
| `update_notice::tests::a_closed_port_and_a_silent_server_cost_at_most_the_timeout` (`apps\rapid\src\update_notice.rs:954`) | elapsed `2.0224438s` against a `< 2s` bound | Windows retries a TCP connect to a closed loopback port for about 2 s before reporting refusal. The product's bound is its configured timeout (3 s in the test); the *test's* 2 s bound encodes a Unix expectation. Decide whether the product should cap connect time lower on Windows, or the test should assert against the configured timeout. |
| **macOS:** `exec_tools::tests::every_line_is_recorded_before_the_monitors_end` (`apps/rapid/src/exec_tools.rs:12316`) | `left: 28  right: 31` | The test name states an invariant: every printed line is recorded before the monitor's end. 28 of 31 means the end was recorded before the last lines. Treat as a **possible product race** (ordering of line records and the end record in the monitor reader), not a test flake, until shown otherwise. It also did not fail on Ubuntu; reproduce under load on macOS or Linux (the other CI machines are slower). |

Unknowns to close: the Windows job runs `Schema fixtures` after `Test`, and it was skipped because `Test` failed — it
may hold further failures. Check after the list above is green.

---

## 5. Tasks for the Windows machine

Do these in order. After each task: format, clippy (`--workspace --all-targets -D warnings`), the affected tests,
revert-cycle, commit, self-review. Run the full suite once at the end of each group, alone.

- **W0 — Reproduce.** Run the full CI test command (§2) on Windows. Confirm the 13 failures above are the same 13 (or
  record the difference). Save the log. Check `git status --porcelain --ignored --untracked-files=all -- .rapidlm`
  afterwards.
- **W1 — Group A (loopback servers).** Fix the four one-shot servers; prove each by running the test repeatedly
  (`for i in $(seq 1 30); do cargo test -p <crate> --lib <name> -- --exact || break; done`), old vs new.
- **W2 — Group B (paths and line endings).** One commit per distinct cause: the `\r\n` cause (find where the `\r`
  enters), the `\\?\` form in `rapid du`, the TOML-escaped policy path. For the policy test also add a product-level
  test that a managed `allowed_sources` rule matches a Windows-style path, if it currently cannot.
- **W3 — Group C (monitor and timing).** Determine why a cancelled monitor is recorded `completed` on Windows (process
  group / job-object termination and exit-status mapping in the job supervisor; compare with how `rapid` stops
  ordinary background jobs on Windows). Fix the product if it is a product defect; add a Windows-side contract test.
  Resolve the update-notice bound as described. Investigate the macOS ordering race (`every_line_is_recorded_before...`)
  as a product bug first.
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
