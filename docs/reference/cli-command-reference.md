# CLI Command Reference — Target V3 Surface

This is the target public command grammar; Phase 0 reconciles it with current source before breaking/renaming existing commands.

> **What the binary actually dispatches today** (everything else in the table below is
> roadmap, not shipped behavior): `exec`, `trust`, `goal`, `mcp`, `doctor`, `plugins`,
> `agents`, `cron`, `scan`, `findings`, `sessions`, `inspect-export`, `insights`, `permissions`,
> `playbook-compile`, `agent-cli`, `mcp-tools`, `tools`, `release-manifest`,
> `completions`, `man`. Those names are checked against the one table in source
> (`interactive::SUBCOMMANDS`) by
> `the_reference_doc_lists_exactly_the_dispatched_subcommands`, which is the same
> table `rapid --help`, `rapid completions` and `rapid man` print from — so this
> paragraph cannot drift from the dispatcher without failing the build. A subcommand
> this reference lists but that table does not carry exits 2 with
> "rapid: unknown subcommand", and `rapid --help` does not advertise it.

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
| `rapid mcp list|get|add|remove|probe` | project MCP servers (stdio); see the MCP section below |
| `rapid plugins validate|register|list|approve|reject|hook-test` | plugin manifest validation, trust ledger (register stores untrusted; only explicit approve grants capabilities), and hook dry-run against a fixture event |
| `rapid permissions list|allow|revoke` | persisted per-project tool grants; see the permissions section below |
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
on an untrusted project, and entries this build cannot run are counted with their
reasons) and never claimed to be verified. `rapid mcp probe` is the command that
actually performs an MCP handshake.

## `rapid permissions` — persisted per-project tool grants

`PermissionLattice::evaluate` consults, in order: managed-policy bans and write-scope
ceilings, plan-mode's write floor, project `permissions` rules, read-only auto-allow,
**persisted per-project grants**, then the mode table. The grant step was implemented and
tested but *unreachable in production* — `parse_grants` was a reader with no counterpart,
so nothing ever created a grant. This command is the writer.

It matters because `PermissionMode::Default` — the out-of-box mode for the interactive
TUI and headless exec alike — resolves every non-read call to "ask", and no surface in
this build can prompt for an approval, so the call is denied. Until that gap is closed, a
grant is the only way to pre-approve one specific tool for one specific project without
widening the permission mode for everything.

| Command | Behavior |
|---|---|
| `list` | Every grant recorded for this project, plus the store path and the project's trust state. |
| `allow <pattern>...` | Records grants. Idempotent — re-granting reports `already-granted` and writes nothing. |
| `revoke <pattern>...` | Removes grants. Exits `1` if nothing matched. |

Patterns are `Tool` or `Tool(arg-glob)` — the same grammar the `permissions` rules in
`.rapidlm/settings.json` use, parsed by the same `ToolPattern::parse`, so this command
cannot write a pattern the loader would reject.

**Boundaries.** A grant only ever narrows the gap between "ask" and "allow": it cannot
widen past a managed-policy tool ban, or past a write-scope ceiling for the file-edit
tools that ceiling governs — both are checked before grants are consulted. (A write-scope
ceiling classifies only file edits, so it never constrained `shell_exec`, with or without
a grant.) A grant does nothing in an untrusted project, where every tool call is refused
outright (the command says so). A pattern naming a tool this build does not provide is
recorded as intent and never matches; `rapid tools` prints the real names. Writing a grant is a privilege escalation, so the
command is reachable only from argv — no model tool, slash command, hook, or
autonomous-goal path dispatches a subcommand. The store lives in the RapidLM home, keyed
by canonical project root, is never wider than owner-only (`0600`) — a narrower mode the
user chose is kept, a wider one is tightened on the next write — and the whole
read-modify-write is held under a sibling `.lock`, so concurrent runs cannot lose each
other's grants. A store that does not parse is refused, never overwritten — the run-time reader
treats an unparsable store as "no grants" (fail-closed, correct there), but a writer that
replaced it would destroy every other project's grants to record one.

## `rapid mcp` — project MCP servers

Manages the servers a project declares under `mcpServers` in `.rapidlm/settings.json`
(and `.claude/settings.json`, which is **read** for compatibility; `add` only ever
creates entries in `.rapidlm/settings.json`, but `remove` deletes from whichever files
define the server — a `remove` that left the server running would be worse than one that
edits a shared file). Every command reads through the same loader the turn path uses, so
this can never report a server a turn would not register, or hide one it would.

| Command | Behavior |
|---|---|
| `list` | Every usable server (name, source file, command, arg count, env *key* names) and every rejected entry with its reason. Read-only; starts nothing. Exits `0` even when entries were rejected — listing is not a diagnosis. |
| `get <name>` | One server's full configuration plus the `mcp__<server>__*` prefix its tools appear under. A name that is configured but rejected reports the rejection, not "not configured". Exits `1` for an unusable or unknown name. |
| `add <name> --command <program> [--arg <v>]... [--env KEY=VALUE]... [--force]` | Writes a stdio entry into `.rapidlm/settings.json` (temp-file-then-rename). Refuses to overwrite an existing entry without `--force`, refuses a name the loader would reject, and refuses to rewrite a settings file it could not parse. Re-reads through the loader afterwards and warns if the new entry still would not run. |
| `remove <name>` | Removes the entry from every project settings file that defines it, naming each. Exits `1` if no file defined it. A settings file it could not read is a warning, not a failure — a commented `.claude/settings.json` must not break `rapid mcp remove`. |
| `probe [<name>]` | Starts the configured server(s) for real — the same spawn, environment, and `initialize`/`tools/list` handshake a turn performs — and reports the tools each advertises. Servers are probed one at a time and one that never answers costs up to 30 seconds each. Exits `1` if any probed server did not come up. |

**Trust.** `probe` executes project-declared commands, so it requires the project to be
trusted, exactly as registration does, and fails closed on an unreadable trust catalog.
`list`/`get` are read-only. `add`/`remove` edit settings files and are reachable only
from this process's argv — no model tool, slash command, or autonomous-goal path
dispatches a subcommand.

**Secrets.** An `env` value is never printed by any command; only its key name is —
including by the `--env` usage error, whose operand *is* the secret in the case it
catches. A settings file this command creates is owner-only (`0600`), because an `env`
value is usually a token; one that already exists keeps whatever mode it has.

**Writes.** `add`/`remove` rewrite the settings file as pretty-printed JSON through a
temp-file-then-rename. Every other key's value is preserved; object key order and
original indentation are not. A settings file that does not parse is refused, never
rewritten. The read-modify-write is not locked, so two concurrent `add`s in the same
project can lose one — an accepted limitation for a human-invoked command.

**Supported transports.** stdio only. An entry with `type`/`url` and no `command` is
reported as an unsupported remote transport rather than being silently ignored.

**Bounds, applied to the merged project view rather than per file.** At most 8 servers
per project; a name must be non-empty, at most 32 bytes, and inside
`agent_runtime::turn::valid_ident`'s alphabet (`[A-Za-z0-9._:-]` — the same set the
composed tool name has to satisfy), and must not contain `__` (the
`mcp__<server>__<tool>` separator — a name containing it would make every one of that
server's tools unroutable). When more than 8 are configured, the ones that fit are chosen
by settings-file order and then by ascending server name, *not* by the order they appear
in the file, and the rest are reported as rejected. A name defined in both settings files
resolves to the first file's entry, and the second is reported as shadowed rather than
being spawned as an unreachable duplicate. Every rejection appears in `rapid mcp list`,
in the `mcp` row of `rapid doctor`, and as a stderr warning on the turn that skipped it;
the per-turn warning is capped at 32 lines plus a count of the rest.
