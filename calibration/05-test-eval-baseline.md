# F. Test / Eval Baseline (read-only verification)

Environment: macOS, Rust toolchain (workspace `rust-version = 1.97.1`), cargo. No network/credentialed/Computer-Use actions were performed. Build artifacts under `target/` are disposable and gitignored.

## Commands executed

| # | Command | Scope | Exit | Result |
|---|---|---|---|---|
| 1 | `cargo check --workspace` | all 29 crates + `apps/rapid` | **0** | compiles end-to-end; 2 trivial unused-import warnings (`kernel/src/ipc/client.rs:15`, `tui/src/panels/agents.rs:13`) |
| 2 | `cargo test --workspace --no-run` | all test targets | **0** | all test targets compile |
| 3 | `cargo test -p kernel -p event-ledger -p capability-broker -p context-engine -p tool-gateway` | 5 core crates | **0** (clean run) | **all pass**: kernel 161, event-ledger 59+5, capability-broker 130+2, context-engine 230+5, tool-gateway 39 (~619 tests, 0 failures) |

## Notable observation: one FLAKY test

- `kernel::ipc::client::tests::stream_disconnect_resumes_by_cursor_and_dedups` is **non-deterministic**.
  - Run A (first execution): `test result: FAILED. 160 passed; 1 failed` — panic at `crates/kernel/src/ipc/client.rs:1559:9`, assertion `left == right` failed (`left: 0, right: 1`), i.e. a resume/dedup count mismatch on stream reconnect.
  - Run B (immediate re-run): `test result: ok. 161 passed; 0 failed` for the same crate — the same test passed.
  - Same total (161) but different pass/fail split ⇒ the test is **flaky**, pointing to a timing/ordering race in the IPC client's cursor-resume + dedup logic. This is the only instability observed across the sampled crates.
- Recorded, **not repaired** (calibration goal forbids modifying implementation). The next goal should stabilize this test and audit the IPC resume/dedup path.

## TS SDK (`sdk/typescript`)
- Not executed in this audit (no Node/pnpm run performed here). `package.json`/`pnpm-workspace.yaml` + `.github/workflows/ci.yml` typecheck and test it in CI; `test/{client,compat,generated,local}.test.ts` exist and are golden-referenced. Status: PRESENT_UNVERIFIED (build/typecheck assumed green via CI; not re-run here).

## What was NOT run (and why)
- Full `cargo test --workspace` (all crates + integration): not executed to conserve time; sampled 5 core crates instead. The un-sampled crates (agent-runtime, security, sandbox, process-supervisor, llm-router, mcp, acp, plugin-host, tui, telemetry, computer-use, mobile-sim, workspace, vcs, auth, protocol, apps/rapid) were audited by source + cited test names by the 8 subsystem subagents, but their test binaries were not executed in this baseline.
- End-to-end / integration suites requiring credentials, network, live LLM, or OS Computer-Use frameworks (macOS AX / Win UIA / Linux AT-SPI / CDP-Playwright): intentionally **not executed** — they would change external state and require resources unavailable/unsafe in a read-only calibration.
- `cargo clippy` and `cargo fmt --check`: covered by CI (`ci.yml` runs `clippy -Dwarnings` + `fmt`); not re-run here.

## Baseline conclusion
The repository is **not a stub scaffold**: the entire workspace compiles and the sampled test suite is green (~619 tests, 0 stable failures). The only reliability concern is one **flaky kernel IPC test** in the (currently unexercised) daemon client path. The `development-ledger.md` `NOT_STARTED` status is therefore a documentation artifact, not a reflection of implementation truth.
