# CLI Command Reference — Target V3 Surface

This is the target public command grammar; Phase 0 reconciles it with current source before breaking/renaming existing commands.

| Command | Purpose |
|---|---|
| `rapid` | interactive TUI |
| `rapid exec [--verbose] <prompt>` | one-shot/headless task (uses the configured model; workspace file tools behind project trust, cause-classified failures; see [model-configuration.md](model-configuration.md) and the exec section below) |
| `rapid run <goal/playbook>` | durable graph run |
| `rapid goal create|show|pause|resume|cancel|complete|budget|verify|evidence|claim` | goal lifecycle; `goal evidence record` citations resolve against the session ledger; `goal complete` is gated on the evidence store; `goal claim --summary S --check crit=cmd [--timeout-secs N]` runs deterministic checks for real, cites each run in the ledger, and only accepts when the host supervisor verifies every criterion |
| `rapid resume [session/run]` | resume durable session/run |
| `rapid fork [checkpoint]` | non-destructive branch |
| `rapid rewind` | restore/fork conversation/graph/workspace checkpoint |
| `rapid daemon` | durable local kernel service |
| `rapid acp` | ACP stdio server |
| `rapid graph show|watch|diff|why-ready|why-blocked|retry|export` | graph inspection |
| `rapid context show|explain|compact|search` | context inspection |
| `rapid evidence show|verify|export` | evidence/verification |
| `rapid agents list|inspect|cancel` | agent attempts |
| `rapid agents list|validate|scaffold` | project agent definitions (`.rapidlm/agents/*.toml`) validated against role tool surfaces |
| `rapid cron add|list|remove|poll` | durable prompt cron over the event ledger (claim-lease firing, corruption quarantine) |
| `rapid process list|logs|input|cancel|monitor` | supervised tasks |
| `rapid computer ...` | computer/browser/mobile actions |
| `rapid sandbox status|doctor` | isolation diagnostics |
| `rapid mcp list|add|remove|auth|refresh` | MCP integration |
| `rapid plugins validate|register|list|approve|reject|hook-test` | plugin manifest validation, trust ledger (register stores untrusted; only explicit approve grants capabilities), and hook dry-run against a fixture event |
| `rapid hooks list|test|enable|disable` | lifecycle hooks |
| `rapid skills list|show|enable|disable` | skills |
| `rapid eval run|compare|report` | evaluation harness |
| `rapid inspect <session/run>` | diagnostic state |
| `rapid export` | transcript/events/graph/evidence bundle |
| `rapid doctor` | offline, read-only diagnosis of environment/home/config/model/context-budget/project/trust/sandbox/git/scanner/hooks/MCP/plugins and security posture (see the doctor section below) |
| `rapid update` | signed updater |

Global flags include project/workspace, session, model/provider, permission mode (`default|dont-ask|accept-edits` subject to policy), sandbox requirement, token/cost/time budgets, JSON/JSONL/no-color/quiet, daemon endpoint and trace verbosity. Privilege-affecting flags cannot exceed organization/user ceilings.

## `rapid exec` — headless task turn

Exit code `0` on success (stdout carries only the answer summary; stderr the token total); every failure exits nonzero.

- **Cause-classified failures**: provider failures name their class and the first remedy instead of one shared `failed` status — `agent turn failed: authentication; check the configured API credential`, `agent turn failed: connection; check network reachability and the configured base_url`, `agent turn failed: provider rejection; check the model id and request shape`, `agent turn failed: transient provider condition; bounded retries were exhausted; try again later`, and `agent turn failed: unspecified failure; no provider-classified detail is available`. Messages never echo credential material or provider bodies.
- **Transient retry**: provider-reported transient conditions (rate limits with any retry-after hint, 5xx) retry automatically at the step layer with bounded backoff (cancellable); a step that already committed tool effects is never replayed; exhausted retries still end as the typed failure above.
- **Workspace tools behind trust**: in a trusted project (`.rapidlm`/`.git` root granted in the project-trust catalog) exec can complete file tasks with `workspace.write` (≤ 8 KiB per call, workspace-scoped, symlink-checked) and `workspace.read` (bounded). Untrusted or unresolvable projects refuse every tool call and fail closed — writes never leave the workspace.
- **Diagnostics opt-in**: `--verbose` adds bounded diagnostic lines on stderr — one `model step host=<endpoint host> attempt=<n> outcome=<ok|cancelled|bound_exceeded|failed:auth|failed:connection|failed:rejected|failed:transient|failed:unspecified> tokens=<t>` per model step plus a final `turn outcome=<status> tokens=<n> [cause=<tag>]`. Host labels are bare host names (never paths or credentials); stdout stays answer-only in both modes, and without the flag output is unchanged.

## `rapid doctor` — environment and project diagnosis

Answers one question: *if I try to use Rapid in this environment and project, what
works, what is broken, and what should I do about it?*

Every check drives the same production code path the real commands do — the same
configuration loader, the same layered model resolution (`RAPIDLM_CONFIG`/`RAPIDLM_MODEL`
overrides, then the user config, then the typed fallback — including the `[models] fallback`
chain and any managed policy), the same model-derived context budget, the same
`canonicalize_dir` → `detect_project_root` → `ProjectIdentity` → `ProjectTrustStore`
chain, and the same tiered sandbox backends. There is no doctor-specific config parser,
model resolver, or budget arithmetic.

**Checks, in report order.** `environment`, `home`, `config`, `model`, `credentials`,
`context-budget`, `project`, `project-trust`, `trust-store`, `workspace-tools`,
`sandbox`, `sandbox-probe`, `git`, `scanner`, `hooks`, `mcp`, `plugins`,
`credential-store`, `project-config-exposure`, `security-policy`, `release-signature`.
The order is fixed, so output is stable across runs and safe to diff or snapshot.

**Status model.** `PASS` · `WARN` · `FAIL` · `SKIP`. The split is decided by one
question only — is the thing checked required for core behavior?

- `FAIL` — mandatory: `home` unresolvable, `config` malformed or an explicitly named
  `RAPIDLM_CONFIG` that does not exist, `model` unresolvable or unconstructable,
  `project` working directory unresolvable, `project-trust`/`trust-store` unreadable or
  corrupt (fails closed), a context budget with no room for input, a malformed
  capability-policy document.
- `WARN` — optional or degraded: untrusted project (and therefore disabled workspace
  tools and unregistered MCP servers), no sandbox backend or only weak isolation, a
  failed sandbox smoke probe, git missing, a configured scanner or hook whose executable
  is absent, an unavailable platform keychain, project-controlled executable config
  present.
- `SKIP` — not applicable: outside a project, nothing configured for that integration,
  or a prerequisite (config, model) was unavailable — never a cascading fake failure.

**Exit code.** `0` when no check failed; `1` when at least one did. Warnings and skips
never fail the command: an absent optional integration is not a broken installation.

**Network policy — offline by default, and there is no opt-in.** No check contacts a
provider or makes a billable model call. Provider configuration is validated locally
(endpoint parsing, capability pinning, credential seeding into a process-local store) and
the model row says `connectivity not tested` rather than implying the endpoint was
reached. An end-to-end test points a configured provider at a listener that records any
connection and asserts it stays untouched.

**Read-only.** Doctor never grants or revokes trust, rewrites configuration, installs,
approves or executes a plugin, runs a hook, or runs a scanner. It reads trust through the
same `ProjectTrustStore::get` every turn uses. The only writes are a uniquely named
temp file proving the RapidLM home is writable, and a scratch directory for the sandbox
smoke probe — both removed immediately, neither inside the project.

**Sandbox probe.** A real execution (`echo`) through the production sandbox abstraction,
using whichever backend this platform's `shell_exec --sandbox` would actually select
(Seatbelt on macOS with `sandbox-exec` present, host-restricted otherwise), in a scratch
directory with the network denied. A failed probe warns; it never fails the command.

**Secret safety.** The whole rendered report is passed through
`security::SecretRedactionRegistry`, seeded with every credential the resolved model
configuration actually produced, so even a provider error that quoted a key verbatim
cannot print it. Credential rows report the *source* production selected (inline
`api_key`, a named `env_key`, or keyless) and never a value.

**Not covered.** Daemon/ACP reachability, MCP server handshakes, plugin execution, and
retrieval/index health are outside this command; where the state is knowable locally it
is reported honestly (for example, MCP servers are listed as configured-but-unregistered
on an untrusted project) and never claimed to be verified.
