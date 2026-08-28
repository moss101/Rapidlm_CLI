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
| `rapid doctor` | environment/config/daemon/provider/sandbox checks |
| `rapid update` | signed updater |

Global flags include project/workspace, session, model/provider, permission mode (`default|dont-ask|accept-edits` subject to policy), sandbox requirement, token/cost/time budgets, JSON/JSONL/no-color/quiet, daemon endpoint and trace verbosity. Privilege-affecting flags cannot exceed organization/user ceilings.

## `rapid exec` — headless task turn

Exit code `0` on success (stdout carries only the answer summary; stderr the token total); every failure exits nonzero.

- **Cause-classified failures**: provider failures name their class and the first remedy instead of one shared `failed` status — `agent turn failed: authentication; check the configured API credential`, `agent turn failed: connection; check network reachability and the configured base_url`, `agent turn failed: provider rejection; check the model id and request shape`, `agent turn failed: transient provider condition; bounded retries were exhausted; try again later`, and `agent turn failed: unspecified failure; no provider-classified detail is available`. Messages never echo credential material or provider bodies.
- **Transient retry**: provider-reported transient conditions (rate limits with any retry-after hint, 5xx) retry automatically at the step layer with bounded backoff (cancellable); a step that already committed tool effects is never replayed; exhausted retries still end as the typed failure above.
- **Workspace tools behind trust**: in a trusted project (`.rapidlm`/`.git` root granted in the project-trust catalog) exec can complete file tasks with `workspace.write` (≤ 8 KiB per call, workspace-scoped, symlink-checked) and `workspace.read` (bounded). Untrusted or unresolvable projects refuse every tool call and fail closed — writes never leave the workspace.
- **Diagnostics opt-in**: `--verbose` adds bounded diagnostic lines on stderr — one `model step host=<endpoint host> attempt=<n> outcome=<ok|cancelled|bound_exceeded|failed:auth|failed:connection|failed:rejected|failed:transient|failed:unspecified> tokens=<t>` per model step plus a final `turn outcome=<status> tokens=<n> [cause=<tag>]`. Host labels are bare host names (never paths or credentials); stdout stays answer-only in both modes, and without the flag output is unchanged.
