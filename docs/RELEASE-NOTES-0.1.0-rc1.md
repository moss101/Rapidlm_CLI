# RapidLM CLI 0.1.0-rc1 — Release Notes (draft)

**Status: release candidate.** Publication is the final approval step; nothing
here is published until that approval lands. All items below are shipped and
validated in this tree (see `docs/gap-to-delivery.md` for the evidence map).

## Highlights

- **Interactive coding that works end to end**: a pending tool call pauses the
  turn, surfaces its action/scope/diff in the TUI, and is resolved with
  approve-once / approve-and-remember / deny — durably, across restarts, with
  the exact action resumed and completed side effects never repeated.
- **`rapid run`**: validated playbooks execute with bounded parallelism,
  human-approval gates, evidence invalidation on code change, and
  restart-safe pause/resume. Four runnable examples ship in `examples/playbooks/`.
- **`rapid acp`** — RapidLM as an ACP agent over stdio (Zed & friends), with
  approvals surfacing as real permission requests.
- **`rapid daemon`** — the TypeScript SDK's wire contract on an owner-only Unix
  socket; the SDK E2E (`examples/daemon/sdk-e2e.mjs`) drives connect → submit →
  stream → fork → resume.
- **MCP over Streamable HTTP** in addition to stdio, with egress authorization
  scoped to the configured origin (`rapid mcp probe` verified live).
- **Worktree isolation for write-capable subagents**, with reviewable
  integrate/abandon (`/agents integrate|abandon`).
- **`rapid eval`** — reproducible benchmark: 40-task suite, offline mechanical
  validation (40/40), live mode with honest skip reporting.
- **Truthful sandbox levels** (`[sandbox: seatbelt — fs+network confined]` vs
  `[host-restricted — resource limits only]`) and `RAPIDLM_SANDBOX_REQUIRED=1`
  fail-closed confinement.
- **Self-update with rollback** (`rapid update`): checksum-verified, smoke-run,
  atomically swapped; any failure restores the old binary.
- **Per-model capability overrides** (`vision` / `caching` / `reasoning`) and
  mid-session `/model select`.

## Known limitations (documented, not hidden)

- Token-level streaming to the UI needs an incremental HTTP transport in
  llm-router; the TUI currently updates per tool step.
- Live competitive evaluation awaits model credentials
  (`docs/credentials-request.md`); offline validation is 40/40.
- Code signing beyond SHA-256 checksums and SBOM provenance attestations
  require signing infrastructure (approval-gated).
- Windows: job control and hooks are Unix-first; the sandbox tiers need Unix tools.

## Platforms

| Platform | Artifact | Status |
|---|---|---|
| macOS aarch64 | `rapid-aarch64-apple-darwin.tar.gz` | built + smoke-tested |
| Linux x86_64 | `rapid-x86_64-unknown-linux-gnu.tar.gz` | built + CI-tested |
| Windows x86_64 | `rapid-x86_64-pc-windows-msvc.zip` | builds/lints; tests informational |

SHA-256 checksums accompany every artifact; `rapid release-manifest` emits the
manifest with an SBOM (CycloneDX from Cargo.lock) and provenance block.

## Upgrading

`rapid update --url <release-manifest-url>` verifies the checksum, smoke-runs
the staged binary, and swaps atomically; any failure restores the previous
binary. First-time users: see `docs/getting-started.md`.
