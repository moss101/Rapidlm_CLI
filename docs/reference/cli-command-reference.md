# CLI Command Reference — Target V3 Surface

This is the target public command grammar; Phase 0 reconciles it with current source before breaking/renaming existing commands.

| Command | Purpose |
|---|---|
| `rapid` | interactive TUI |
| `rapid exec <prompt>` | one-shot/headless task (uses the configured model; see [model-configuration.md](model-configuration.md)) |
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
