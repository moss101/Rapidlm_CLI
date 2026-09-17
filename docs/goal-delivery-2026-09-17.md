# Goal delivery — Windows runtime gate, live browser driver, GVS5H Phase 0 (2026-09-17)

Baseline: `22bdc48` (the last commit of the 2026-09-15 delivery). Delivery commits, in order:
`aa6efda`, `e3eef31`, `8682059`, `77e279f`, `21b90b7`, `0f38049`, `7aeb2bb`, `e2ae513`, `3cede29`,
`63c81c5`, and the merge `fc5dfbc` (PR #1: `9af8a44`, `4474298`, `86a9dbc`).

## 1. Windows runtime tests are a gate

| Criterion | Status | Evidence |
|---|---|---|
| The Windows Test step is not `continue-on-error` | done | `.github/workflows/ci.yml` (`0f38049`); the step's failure is a red job like Linux/macOS |
| The full workspace suite passes on Windows | done | CI run 35163406404 (`e2ae513`): `rust (windows-latest)` Test success — 0 failing tests in ~2,900; 175 failing at the baseline (`22bdc48`, run 35020296597, full log via the jobs API) |
| Failures were fixed in the product where the product was wrong | done | Verbatim-path handling (`protocol::host_path`, `aa6efda`); `~/.rapidlm` mistaken for a project marker on every platform (`is_project_marker`, `aa6efda`); cancelled jobs reported as "completed exit 1" (`JobShared.killed`, `77e279f`); child base environment (`protocol::host_env`, `77e279f`); `taskkill` without `SystemRoot` (`aa6efda`); glob separators; update staging names; check-command quoting; `resolve_program` PATHEXT; simctl drive prefixes; headless viewport probe |
| POSIX-only behaviour is typed, not emulated | done | `PtyError::Unsupported`; `HostRestrictedBackend::health` → `HealthReason::PlatformUnsupported` on evidence; commit-scanner gate and `rapid scan` fail closed; each with a `cfg(not(unix))` contract test (`pty_is_a_typed_unsupported_error_off_unix`, `unavailable_without_posix_governance`, `git_commit_is_blocked_when_the_configured_scanner_cannot_run_here`, `without_a_sandbox_a_configured_scanner_blocks_the_gate`, `run_sandboxed_reports_the_tier_unavailable_off_unix`) |
| Test fixtures are portable | done | `crates/test-fixtures` (`tool`, `tool_static`, `sh_quote`, `find_on_path`, `process_alive`, `processes_mentioning`); no `/bin/…` literal remains in a test that runs on Windows |
| Documentation states the platform contract | done | `docs/getting-started.md` "Windows" paragraph (`0f38049`, corrected in `3cede29`) |
| Self-review | done | Two background adversarial reviews; all verified findings fixed in `e3eef31` and `3cede29`/`63c81c5` |

## 2. Live browser driver

| Criterion | Status | Evidence |
|---|---|---|
| A real implementation of `PlaywrightBackend` / `PageCapture` / `PageActor` | done | `crates/computer-use/src/browser/cdp.rs` `ChromiumCdpBackend` (Chrome/Chromium/Edge over CDP; `browser::ws` loopback WebSocket client) |
| Real observe → authorize → act → verify → recover on a live browser | done | `crates/computer-use/tests/live_chromium.rs::a_real_chromium_is_observed_driven_and_verified_end_to_end` — navigate, observe (labelled textbox, button, sensitive password field with a withheld name), type, click, verify through `verify()`, Backspace, scroll, redacted screenshot on the sensitive page and a real PNG on the next, link click → page-initiated navigation → the old observation refused as `StaleObservation` through the browser-reported loader identity, cookies (server-set and agent-set), localStorage, trace artifact. Green on Ubuntu, macOS and Windows (run 35167476607) |
| Untrusted page content cannot influence privileged inputs | done | URL and document identity from `Page.getFrameTree`, collector and actions in an isolated world, bounded text, dialogs auto-answered, secret handles refused (`86a9dbc`) |
| A user-reachable surface | done | `rapid browser <url> --step … [--screenshot] [--json]` (`apps/rapid/src/browser_cli.rs`), listed in `docs/reference/cli-command-reference.md`'s shipped-commands paragraph and `rapid --help` |
| Non-Chromium engines | typed unavailable | `BrowserEngine::Firefox`/`Webkit` → `BrowserSessionError::Unavailable` (`a_second_engine_request_is_a_typed_unavailable_not_a_chromium_in_disguise`) |
| Model-facing browser tools (`browser_observe`/`browser_act` for the agent runtime) | not delivered | The driver and the CLI are the product surface; wiring the agent runtime's tool surface to it needs the permission lattice's browser class and is a separate piece of work |

## 3. GVS5H Phase 0

| Criterion | Status | Evidence |
|---|---|---|
| GVS-001 baseline audit | done | `docs/goals/gvs5h-phase0-baseline-2026-09-17.md` (`7aeb2bb`) |
| GVS-002 architecture and compatibility contracts | done | `docs/adrs/0021-verified-orchestration-extends-graph-and-supervisor.md` |
| GVS-003 preregistration | done | `docs/evaluation-specs/gvs5h-experiment-preregistration.md` |
| Phases 1–7 | pending | Reordered by the audit (production caller first); nothing implemented |

## 4. Explicitly not delivered (unchanged from 2026-09-15, by design)

Durable child-session continuation; tool auto-repair production wiring; smart phase routing.
