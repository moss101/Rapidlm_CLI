# CLI Command Reference — Target V3 Surface

This is the target public command grammar; Phase 0 reconciles it with current source before breaking/renaming existing commands.

> **What the binary actually dispatches today** (everything else in the table below is
> roadmap, not shipped behavior): `exec`, `run`, `acp`, `daemon`, `eval`, `update`, `trust`, `goal`, `mcp`, `setup`, `doctor`, `plugins`,
> `agents`, `cron`, `loop`, `scan`, `browser`, `findings`, `sessions`, `usage`, `du`, `worktree`, `inspect-export`, `insights`, `permissions`,
> `update`, `resume`, `skills`,
> `playbook-compile`, `agent-cli`, `mcp-tools`, `tools`, `release-manifest`,
> `completions`, `man`. Those names are checked against the one table in source
> (`interactive::SUBCOMMANDS`) by
> `the_reference_doc_lists_exactly_the_dispatched_subcommands`, which is the same
> table `rapid --help`, `rapid completions` and `rapid man` print from — so this
> paragraph cannot drift from the dispatcher without failing the build. A subcommand
> this reference lists but that table does not carry exits 2 with
> "rapid: unknown subcommand", and `rapid --help` does not advertise it.
>
> Hooks have no subcommand: they are project settings (`.rapidlm/settings.json`
> `"hooks"`), narrowed by the managed policy's `[hooks]` table — see
> [hooks](hooks.md). `rapid doctor` lists them; `rapid plugins hook-test`
> dry-runs a plugin hook.

| Command | Purpose |
|---|---|
| `rapid` | interactive TUI |
| `rapid exec [--verbose] <prompt>` | one-shot/headless task (uses the configured model; workspace file tools behind project trust, cause-classified failures; see [model-configuration.md](model-configuration.md) and the exec section below) |
| `rapid run <playbook.json> [--parallel N] [--orchestration off\|verified]` | durable playbook run (`--resume`/`--status`/`--resolve`/`--retry`); with `--orchestration verified` (or `orchestration.mode = "verified"` in config, `RAPIDLM_ORCHESTRATION_MODE=verified`; CLI > env > user > workspace) the same steps run as a Runtime Graph in the project ledger — every step transition a durable `graph.*` event, the verification steps a host supervisor's contract, and `verified: true` meaning the supervisor accepted (every verification step passed in this invocation). A playbook without a verification step is refused in that mode. Experimental; off by default and the default path is unchanged |
| `rapid goal create|show|pause|resume|cancel|complete|budget|verify|evidence|claim` | goal lifecycle; `goal evidence record` citations resolve against the session ledger; `goal complete` is gated on the evidence store; `goal claim --summary S --check crit=cmd [--timeout-secs N]` runs deterministic checks for real, cites each run in the ledger, and only accepts when the host supervisor verifies every criterion |
| `rapid resume [session-id]` | reopen the TUI on an existing session, rebuilt from this project's event ledger; with no id, the session with the most recent activity |
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
| `rapid loop add <interval> <prompt> [--for <lifetime>]|list|rm <id>` | a prompt scheduled every interval (`5m`, `2h`, `1d`); expires after seven days unless `--for` says otherwise (at most 30 days); at most 50 active. Without `--session`, fired like a cron job by `rapid cron poll`; with it, by that session's terminal UI while the session is open (as `/loop` adds one), as a background read-only turn whose result arrives as a notice |
| `rapid process list|logs|input|cancel|monitor` | supervised tasks |
| `rapid computer ...` | computer/browser/mobile actions |
| `rapid sandbox status|doctor` | isolation diagnostics |
| `rapid mcp list|get|add|remove|probe|install` | project MCP servers (stdio and streamable HTTP), and installing one into another program's configuration; see the MCP section below |
| `rapid plugins validate|register|list|approve|reject|hook-test` | plugin manifest validation, trust ledger (register stores untrusted; only explicit approve grants capabilities), and hook dry-run against a fixture event |
| `rapid permissions list|allow|deny|revoke` | persisted per-project tool answers (grants and "never allow"); see the permissions section below |
| `rapid skills list|show <id>|discover [--roots <path>...]` | the skills the loader finds (user `<home>/skills`, the `[skills] extra_dirs` of the user config, and `.rapidlm/skills` then `.agents/skills` of a trusted project) with their root and who may invoke them, what a higher root hides, what was refused and why, what a rule or the managed policy refuses, and the bounds on the model's own loads; one skill's description, flags and instructions; and directories that look like a skills directory, found by shape, with the config line to read one; read-only and offline. See `docs/reference/skills.md` |
| `rapid eval --offline\|--live [--grant-shell] [--trials <n>] [--arms <names>] [--suite <dir>] [--scratch <dir>]` | evaluation harness (shipped): three suites (`eval/suite` representative, `eval/suite-smoke` mechanical smoke, `eval/suite-heldout` held-out), grading v2 outside the agent workspace (protected-file integrity, verification, mutation checks), typed infrastructure skips via a health probe, per-arm accounting with coverage/lower-bound flags, full provenance; exit 0 only when every task of every requested arm passed — see `eval/README.md` |
| `rapid usage [session-id] [--project] [--since <time>] [--output tsv\|json] [--quiet]` | what model steps consumed and cost, per turn, reduced from the ledger's `model.completed` records; a cost the provider did not report is `unknown`, never zero, and `basis` says whether tokens were reported or estimated |
| `rapid du [--reclaim-plan] [--output text\|json]` | bytes RapidLM keeps for this project in areas that never overlap — its `.rapidlm` tree, the user's RapidLM home, the worktrees and the rest of the worktree store under `.git/rapidlm` — each with its top-level entries (file lengths; symlinks not followed); `--reclaim-plan` adds the worktrees `rapid worktree reclaim` would remove, the same lines as its `--dry-run`, and deletes nothing |
| `rapid worktree list\|reclaim [--dry-run]\|abandon <view>` | this project's worktrees (`rapid exec --worktree`, `rapid goal create --worktree`) and whether each may be reclaimed; `reclaim` removes only abandoned or merged, clean worktrees no running session or current goal holds, each removal journaled as `workspace.reclaim` (`--dry-run` removes nothing); `abandon` discards a worktree's changes, journaled as `workspace.abandon` |
| `rapid inspect <session/run>` | diagnostic state |
| `rapid export` | transcript/events/graph/evidence bundle |
| `rapid doctor [--live]` | offline (unless `--live`), read-only diagnosis of environment/home/config/model/context-budget/project/trust/sandbox/git/scanner/hooks/MCP/plugins and security posture (see the doctor section below) |
| `rapid update` | signed updater |

Global flags include project/workspace, session, model/provider, permission mode (`default|dont-ask|accept-edits` subject to policy), sandbox requirement, token/cost/time budgets, JSON/JSONL/no-color/quiet, daemon endpoint and trace verbosity. Privilege-affecting flags cannot exceed organization/user ceilings.



## `web_search`

A model tool, available once the user config names a backend:

```toml
[toolset.web_search]
backend = "json"                        # POST {"q","count"} → {"results":[{url,title,snippet}]}
endpoint = "https://search.example/api"
allowed_domains = ["docs.rs"]           # optional: only these and their subdomains
excluded_domains = ["example.org"]      # optional: never these
max_results = 5                         # 1..10
```

Results come back fenced as untrusted, bounded (snippets 300 characters), filtered by the
domain lists (the omitted count is said). Every request goes through the egress gate; each
decision — for `web_search` and `web_fetch` — is appended to the project's
`.rapidlm/egress-receipts.jsonl`. With no backend configured the tool reports it is
unavailable.
## Agent mode

Before the subcommand, or through the `RAPIDLM_*` mirror named beside each (a flag wins
over its mirror): `--output json|text` (`RAPIDLM_OUTPUT`), `--quiet` (`RAPIDLM_QUIET`),
`--non-interactive` (`RAPIDLM_NON_INTERACTIVE`), `--dry-run` (`RAPIDLM_DRY_RUN`), `--yes`
(`RAPIDLM_YES`), `--no-color` (`RAPIDLM_NO_COLOR`, or `NO_COLOR`), `--timeout <secs>`
(`RAPIDLM_TIMEOUT`). A flag is never silently ignored: a command's own JSON, quiet or
dry-run form is used where it has one (`usage`, `du`, `setup`, `browser`, `worktree`,
`mcp`); `--dry-run` on a command that only reads is accepted; on one that changes state
without a dry run it is refused (exit 2) before anything runs; `--non-interactive` refuses
the TUI (`rapid`, `rapid resume`). In agent mode a command with a JSON form writes JSON
when stdout is not a terminal. `--timeout` stops the run with exit 130. In JSON mode a
failure is written on stderr as `{"error":{"code","message","hint"}}`
(`protocol::cli::CliError`); otherwise the message is followed by `hint: <next command>`.
Exit codes are unchanged.
## `rapid exec` — headless task turn

Exit code `0` on success (stdout carries only the answer summary; stderr the token total); every failure exits nonzero.

- **Cause-classified failures**: provider failures name their class and the first remedy instead of one shared `failed` status — `agent turn failed: authentication; check the configured API credential`, `agent turn failed: connection; check network reachability and the configured base_url`, `agent turn failed: provider rejection; check the model id and request shape`, `agent turn failed: exhausted provider quota; check the provider account's quota or billing` (a 402, or a 429 reporting an exhausted quota — never retried), `agent turn failed: proxy authentication; check the user and password in the proxy variable` (a proxy's 407 — never retried), `agent turn failed: transient provider condition; bounded retries were exhausted; try again later`, and `agent turn failed: unspecified failure; no provider-classified detail is available`. Messages never echo credential material or provider bodies.
- **Transient retry**: provider-reported transient conditions (rate limits with any retry-after hint, 5xx) retry automatically at the step layer with bounded backoff (cancellable); a step that already committed tool effects is never replayed; exhausted retries still end as the typed failure above.
- **Workspace tools behind trust**: in a trusted project (`.rapidlm`/`.git` root granted in the project-trust catalog) exec can complete file tasks with `workspace.write` (≤ 8 KiB per call, workspace-scoped, symlink-checked) and `workspace.read` (bounded). Untrusted or unresolvable projects refuse every tool call and fail closed — writes never leave the workspace.
- **Diagnostics opt-in**: `--verbose` adds bounded diagnostic lines on stderr — one `model step host=<endpoint host> attempt=<n> outcome=<ok|cancelled|bound_exceeded|failed:auth|failed:connection|failed:rejected|failed:quota|failed:proxy_auth|failed:transient|failed:unspecified> tokens=<t>` per model step plus a final `turn outcome=<status> tokens=<n> [cause=<tag>]`. Host labels are bare host names (never paths or credentials); stdout stays answer-only in both modes, and without the flag output is unchanged.

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

**Checks, in report order.** `environment`, `home`, `config`, `managed`, `model`, `credentials`,
`context-budget`, `project`, `project-trust`, `trust-store`, `workspace-tools`,
`sandbox`, `sandbox-probe`, `git`, `scanner`, `hooks`, `mcp`, `plugins`, `skills`,
`credential-store`, `project-config-exposure`, `security-policy`, `release-signature`.
The order is fixed, so output is stable across runs and safe to diff or snapshot.

**Status model.** `PASS` · `WARN` · `FAIL` · `SKIP`. The split is decided by one
question only — is the thing checked required for core behavior?

- `FAIL` — mandatory: `home` unresolvable, `config` malformed or an explicitly named
  `RAPIDLM_CONFIG` that does not exist, `model` unresolvable or unconstructable,
  `project` working directory unresolvable, `project-trust`/`trust-store` unreadable or
  corrupt (fails closed), a context budget with no room for input, a malformed
  capability-policy document, a managed policy that does not load or whose required
  signature does not verify (`managed`; see
  [managed-policy-signature.md](managed-policy-signature.md)).
- `WARN` — optional or degraded: untrusted project (and therefore disabled workspace
  tools and unregistered MCP servers), no sandbox backend or only weak isolation, a
  failed sandbox smoke probe, git missing, a configured scanner or hook whose executable
  is absent, an unavailable platform keychain, project-controlled executable config
  present.
- `SKIP` — not applicable: outside a project, nothing configured for that integration,
  or a prerequisite (config, model) was unavailable — never a cascading fake failure.

**Exit code.** `0` when no check failed; `1` when at least one did. Warnings and skips
never fail the command: an absent optional integration is not a broken installation.

**Network policy — offline by default; `--live` opts in.** Without `--live` no check
contacts a provider or makes a billable model call. Provider configuration is validated
locally (endpoint parsing, capability pinning, credential seeding into a process-local
store) and the model row says `connectivity not tested` rather than implying the endpoint
was reached. An end-to-end test points a configured provider at a listener that records
any connection and asserts it stays untouched.

`rapid doctor --live` then probes every configured `[model.<id>]` — the model a run would
dial for that profile, managed gates applied: as the default when a run may select it,
and, under a managed `locked_default`, as the `[models] fallback` entry or `[phases]
compact` model a run still dials it as — with the request `rapid setup` verifies with (at most 16 output
tokens), through that profile's key and proxy (`[network] proxy`) and on an egress gate
for exactly its endpoint. The probes run side by side, so the command takes about as long
as the slowest (each connection and each answer is bounded by the transport's timeouts).
It adds one row per profile after the offline rows, in profile order: `live:<id>` `PASS`
when the endpoint answered; `FAIL` with the class (`auth`, `quota`, `network`, `server`,
`invalid`), what happened, the next command and the egress receipt (`egress: allowed
<scheme>://<host>:<port>`, or what was refused and why) when the probe failed; `FAIL`
with the reason and no receipt when the profile's configuration does not resolve, so
nothing was sent; `SKIP` when the managed policy refuses the profile, or locks the
default elsewhere and a run does not dial the profile at all (or refuses it in the role
it would dial it in). No configured model is one `live`
`WARN` pointing at `rapid setup`. A failed probe fails the command (exit `1`). Nothing is
written: the receipt is reported, not recorded.

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
| `list` | Every grant and "never allow" answer recorded for this project, then every rule in effect here with its origin (`rule=<effect> <pattern> origin=persisted\|settings:<file>\|managed`), plus the store path and the project's trust state. |
| `allow <pattern>...` | Records grants. Idempotent — re-granting reports `already-granted` and writes nothing. |
| `deny <pattern>...` | Records "never allow" answers: a matching call is refused without asking, ranked after managed policy and before the project's own rules, every grant and every mode — read-only calls included. The newer answer replaces an allow of the same pattern. A store that exists but cannot be read refuses every call until it is fixed. |
| `revoke <pattern>...` | Removes grants or "never allow" answers. Exits `1` if nothing matched. |

Answering an approval can record the same: `/approvals approve <n> remember` a grant, `/approvals deny <n> never` a "never allow" (both for the ask's own standing pattern — a file, an MCP tool, a fetch domain — and refused where it has none, such as a shell command); in ACP, "Allow always" and "Reject always".

Patterns are `Tool` or `Tool(arg-glob)` — the same grammar the `permissions` rules in
`.rapidlm/settings.json` use, parsed by the same `ToolPattern::parse`, so this command
cannot write a pattern the loader would reject.

**For one run.** `rapid exec --allow <pattern>` (repeatable) adds grants for that run
only, consulted where persisted grants are — so managed policy (a banned tool, the
`max_permission_mode` ceiling), plan mode, deny and ask rules and "never allow" answers
still outrank it. The mode a run starts in is `RAPIDLM_PERMISSION_MODE`, else the user's
`[permissions] default_mode` in `config.toml`, else the project's settings, else
`default` — always narrowed to managed policy's ceiling.

**Shell commands.** A `shell_exec(glob)` pattern matches a command's joined argv. When
the call runs a shell's script (`sh -c`, `bash -lc`, …), the script is parsed — quotes,
`;`, `&&`, `||`, pipelines, `&`, subshells, leading `NAME=value` assignments, nested
`sh -c` — and judged command by command: a deny or ask pattern (or a "never allow", or a
managed ban) matching any one command decides, and an allow pattern or grant allows only
when every command is covered. A quoted `;` is an argument, not a second command. A script
that cannot be read whole — a `$(…)` or backtick substitution, a heredoc, process
substitution, a compound command (`if`, `for`, `{ … }`), `eval`/`source` — is never
allowed by a pattern; the mode decides (in `default`, it asks). In `auto` mode a call made
only of `mkdir`, `touch`, `ls` and `pwd`, with no redirect or assignment, runs without
asking; in every other mode those ask like any command.

**In the TUI.** `/permissions` prints the same report and names any tool this session
already saw denied; `/permissions allow <pattern>` and `/permissions revoke <pattern>`
write through the very same code path, so there is one writer and one grammar. A grant is
the user's own approval, so — unlike `/mcp remove` — it carries no approval gate of its
own, for the same reason `rapid trust grant` does not.

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
| `list` | Every usable server (name, source file, command, arg count, env *key* names; a remote server adds `transport=http auth=…`, naming its credential sources and never their values) and every rejected entry with its reason. Read-only; starts nothing. Exits `0` even when entries were rejected — listing is not a diagnosis. |
| `get <name>` | One server's full configuration, its resolved `startup-timeout-ms=` and `tool-timeout-ms=` each with the layer it came from (`source=built-in`, `project settings`, `user config [mcp]`, `environment` or `managed policy`, and `lowered-from=<ms>` when the managed ceiling cut the call limit), `deferred=true|false source=…` (the entry, the user's `[mcp] default_deferred`, or the built-in `false`), plus the `mcp__<server>__*` prefix its tools appear under. A name that is configured but rejected reports the rejection, not "not configured". Exits `1` for an unusable or unknown name. |
| `add <name> --command <program> [--arg <v>]... [--env KEY=VALUE]... [--force]` | Writes a stdio entry into `.rapidlm/settings.json` (temp-file-then-rename). Refuses to overwrite an existing entry without `--force`, refuses a name the loader would reject, and refuses to rewrite a settings file it could not parse. Re-reads through the loader afterwards and warns if the new entry still would not run. |
| `add <name> --url <https-url> [--bearer-env VAR] [--header-env NAME=VAR]... [--force]` | Writes a remote entry (`"type": "http"`). The command line names the environment variables that hold a token or header value and never takes one: `--bearer-env` becomes `bearer_token_env`, `--header-env` becomes a `headers_env` entry. `http://` is accepted only for a local address, a URL with `user:password@` is refused, and `--url` excludes `--command`, `--arg` and `--env`. The same refusals and re-read as a stdio `add` apply. |
| `add … [--enabled-tool <pattern>]... [--disabled-tool <pattern>]...` | Writes `enabled_tools` / `disabled_tools` into the entry (see **Tool filters**). The patterns are checked by the loader's own rules before anything is written. |
| `add … [--deferred]` | Writes `deferred: true` into the entry (see **Deferred definitions**). |
| `add … [--tool-timeout-ms <ms>] [--startup-timeout-ms <ms>]` | Writes `tool_timeout_ms` / `startup_timeout_ms` into the entry (see **Timeouts**). A value outside the bounds, or given twice, is refused before anything is written. |
| `remove <name>` | Removes the entry from every project settings file that defines it, naming each. Exits `1` if no file defined it. A settings file it could not read is a warning, not a failure — a commented `.claude/settings.json` must not break `rapid mcp remove`. |
| `install <name> --into <path> [--format jsonc\|toml\|yaml] [--key <a.b>] [--dry-run]` | Puts one of this project's servers into another program's configuration file. Only that entry is written; comments, ordering and every other key are left as they were (JSONC and YAML through small editors that refuse what they do not understand, such as a YAML list of servers; TOML through `toml_edit`). A timestamped `.bak` is kept beside the file, the write is atomic and `0600`, one install runs at a time per user (`<home>/mcp-install.lock`), a second identical run reports `unchanged`, and `--dry-run` prints the diff — `env`/`headers` values shown as `<redacted>` — and writes nothing. |
| `install --discover` | Lists the files in the home and XDG configuration roots (or the path list in `RAPIDLM_CONFIG_ROOTS`) whose *shape* declares an MCP servers map (`mcpServers`, `mcp_servers`, `servers` or `context_servers`, members naming a `command` or `url`), by path — never by a product name. |
| `probe [<name>]` | Starts the configured server(s) for real — the same spawn, environment, and `initialize`/`tools/list` handshake a turn performs — and reports the tools each advertises. Servers are probed one at a time and one that never answers costs up to its start-up limit (30 seconds unless set; see **Timeouts**) each; a server that did not finish starting in time reports `reason=startup_timeout`. Exits `1` if any probed server did not come up. |

**Tool filters.** A server entry may carry `enabled_tools` (an allowlist; absent means
every tool) and `disabled_tools` (applied after it). A pattern is 1–128 bytes of letters,
digits and `. _ : - * ?`, matched against the server's *own* tool name (not
`mcp__<server>__<tool>`): `*` is any run of characters (even none), `?` exactly one,
the rest literal, anchored to the whole name and case-sensitive; each list holds at most 64.
A tool outside the effective set is dropped when the server's tools are listed: it is never
offered to the model and never counted toward the tool limits, and a call to it returns
`mcp_tool_not_enabled` without contacting the server, before any approval is asked or hook
run. For a server that has a filter this also closes the gap where a name the model was
never offered was still callable: a name the server never listed is refused as well (a
server with no filter is forwarded names as before). A tool that is in the set but left out
of a turn by the 20 KiB cap stays callable, as before. A filter only removes: a tool the managed
policy bans (`denied_tools`) or a server it denies stays refused whatever `enabled_tools`
says. A list that is not a list of valid patterns rejects the entry, with the reason in
`rapid mcp list`, `rapid doctor` and the per-turn warning; an unknown key in an entry is
still ignored. A name whose entry asks for fewer tools (or its own limits) and was rejected,
for whatever reason, still belongs to that file: a plain entry of the same name in the second
settings file is not run in its place, and says so. `list` shows `enabled_tools=…` / `disabled_tools=…` for a server that has them (an
empty allowlist reads `(none)`: it offers nothing), `get` prints one `enabled-tool=` /
`disabled-tool=` line per pattern (and `enabled-tools=(none: …)` for an empty allowlist), and `probe` reports `tools=<offered> of=<reported>` and
`matches-nothing=<server> pattern=<p>` for a pattern that names no tool the server reports (a
typo is visible; `(no tool enabled)` when the filter removed everything). A running session
follows a change to a server's filter at its next turn, tightening or loosening, without
starting the server again; an entry that has since become invalid keeps what it had until the
session ends. A tool the filter removes is refused before anything asks about it whether
the server is up or down, and a server brought back to a fuller set of tools gets its block
back where its registration puts it. A pattern that starts with `-` can only be written into
the settings file by hand: `add` refuses it, so that `--disabled-tool --force` does not take
`--force` for a pattern. `add --force` replaces the entry as the command line says and names any
filter list it did not carry over; `install` does not copy the lists to another program's
file, and says so.

**Deferred definitions.** A server's tools cost the model a full definition each, on every
step. A server entry may carry `deferred: true` (or the user's `[mcp] default_deferred = true`
defers every server that does not say; an entry's own `false` wins). The tools in the
server's effective set are then left out of the tool list. In their place the context
carries a **manifest**, one block per deferred server fenced as untrusted under the locator
`mcp-manifest:<server>`: a line per tool (`mcp__<server>__<tool> — ` and the first line of its
description cut to 100 bytes), at most 4 KiB for a server and 8 KiB for all, the rest counted
(`+N more — use mcp_find`); a server with nothing enabled says `(no enabled tools)`, one that
is down shows its `offline` marker. While a deferred server is connected two tools are on
offer, read-only and never asked about in any mode, plan mode included (a deny rule or a
managed ban still applies): `mcp_find` `{query?, server?, limit?}` searches the effective tools
of the deferred servers by name and description (no model, no network): a query is split into
lowercase alphanumeric tokens and a tool scores 3 for each token its own name contains and 1
for each its description contains, tools scoring nothing are left out unless the query is
empty (then the first tools in name order), highest score first and then by name, at most 20
(10 by default), each as its full name and a description cut to 200 bytes, fenced as untrusted;
and `mcp_load` `{names: [full tool name, …]}` (at most 16) answers each name with `loaded`,
`already_loaded`, `unknown`, `not_enabled` (the settings filter it out) or `too_large` (it
would break the 20 KiB cap on MCP definitions or a limit of the provider), then one
`evicted:<name>` for each tool unloaded to make room, listing the parameter names of the
tools just loaded. A loaded tool is in the tool list from the next step of the same turn, with
its full definition. A call to a deferred tool that is not loaded returns
`mcp_tool_not_loaded` without contacting the server and without asking; the model is never
loaded for. A loaded tool is called like any other: it still asks or is denied by the mode and
the rules, and its hooks run. The loaded set holds at most `[mcp] max_loaded_tools` tools
(default 24, at most 1024; held to what the provider takes beside the built-in tools and the
two finder tools, and `rapid doctor` says when it was): a load that would exceed it unloads
the least recently *called* loaded tool (among never-called ones, the one loaded longest ago),
never one the same request loaded. Each load and unload is a ledger record
(`mcp.tools_loaded`, `mcp.tools_unloaded`: names and schema digests, never text), written before
the next tool list is built, and the loaded set is rebuilt from the ledger when the session is
not the one it was last for — after a restart, a resume, a fork or a rewind. At the start of a
turn each deferred server is listed again: a loaded tool whose schema digest changed or that the
server no longer reports is unloaded (`stale`), and one whose server is down is unloaded
(`server_offline`), each with a line in the transcript. Compacting the conversation unloads
nothing. A child does not inherit the parent's loaded set. Nothing is deferred where a load
cannot be put on record (a workflow step, a child, a run that is not recorded): such a server's
tools are listed in full, as before. With no `deferred` anywhere, nothing here exists.

**Timeouts.** A server gets a limit to start (each wait while it starts, the `initialize`
handshake and then the tool listing, may take that long) and a limit to answer one call. Both are 30 000 ms unless a layer says
otherwise; the highest layer that says anything wins: the environment
(`RAPIDLM_MCP_STARTUP_TIMEOUT_MS`, `RAPIDLM_MCP_TOOL_TIMEOUT_MS`), then the `[mcp]` table of
the user's `config.toml` (`startup_timeout_ms`, `tool_timeout_ms`), then the server's own
`startup_timeout_ms` / `tool_timeout_ms` in the project's settings, then the built-in.
A project's value is the lowest layer on purpose: someone who needs a longer limit for a
project's server sets it in their own config. Start-up is 1 000–120 000 ms and a call
1 000–600 000 ms; an entry with a value outside that (or that is not a whole number) is
rejected, with the reason in `rapid mcp list`, `rapid doctor` and the per-turn warning,
and keeps its name, so a plain entry of the same name in the second settings file does not
run in its place. An unusable value in the environment or the user's config is left out and
named (`warning: MCP limits: …`) in `list`, `get`, `probe`, `rapid doctor` and the per-turn
warnings, and the layers below it apply. A managed `[mcp] max_tool_timeout_ms` (any positive number of milliseconds) is a ceiling
on the call limit: a larger resolved limit is lowered to it and reported like the other
managed gates. When a call is not answered in time the model reads `mcp_timeout: <tool> did not answer within <n> ms`,
the host sends the server a `notifications/cancelled` for that request (best effort, and only
for a timeout: a call you interrupt yourself is not announced; a pipe to a server that has
stopped reading cannot be written to with a deadline), and the call's timer is stopped and gone
when the call returns. The limit is the server's time: the time a call spends waiting for your
answer to its request for input is not charged to it. A remote call is held to its limit as a
whole, name lookup and connecting included. The limit also
bounds each wait for a frame, so a call may be given longer than the handshake was. A server
that does not finish starting in time is offline with the reason `startup_timeout`. A running
session follows a changed call limit at its next turn; a start-up limit applies to a server
the next time it starts. `add --force` names any limit it did not carry over, and `install`
does not copy them to another program's file.

**Trust.** `probe` executes project-declared commands, so it requires the project to be
trusted, exactly as registration does, and fails closed on an unreadable trust catalog.
`add` and `install` require it too: an untrusted project's `add` is a refusal that
writes nothing.

**Managed policy.** `[mcp] allowed_servers`/`denied_servers` and `[plugins]
allowed_sources` in the managed policy decide which servers may be added, bound and
installed; `[mcp] max_tool_timeout_ms` caps how long any call may take. An allowed pattern admits a server only by what it runs (its command as
written, or its URL); a denied pattern refuses by name, command, command file name or
the whole command line. `list`, `probe` and `rapid doctor` name what the policy blocks.
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

**Supported transports.** stdio (an entry with a `command`) and streamable HTTP (`type`
set to `http`, with a `url`). An entry of any other `type` is reported as an unsupported
remote transport rather than being silently ignored.

A remote entry authenticates with a static credential. `rapid mcp add --url` never takes
a token, only the names of variables that hold one. In the settings file `bearer_token_env`
names the environment variable that holds a bearer token, `headers_env` maps a header name
to a variable that holds its value, and a literal `headers` value (including
`Authorization`) is sent as written. Variables are read when the server connects; a missing
or empty one is reported by name, never by value. `bearer_token_env` cannot be combined with
an `Authorization` header.

A project file chooses both the variable and the URL it is sent to, so a project-declared
server may read only a variable whose name starts with `MCP_`, or one the **user** lists in
`RAPIDLM_MCP_CREDENTIAL_ENV` (comma-separated, in their own environment, where a project
file cannot write). Any other name is refused with a message that says so, before any
request is made. A credential (`bearer_token_env`, `headers_env`, or an `Authorization` or
key/token-named header) on a plain `http://` URL is refused unless the host is `localhost`
or a loopback or private IPv4 address. Whatever credential a connection sends is removed from
what the server's replies show the model (tool results, before they are cut to length, and
tool descriptions). A remote server cannot put a request for input in front of the user: the
session does not declare it, and such a request is answered with an error. `list` and `get`
show a remote URL without its query and fragment, which can carry a token.

A server that answers `401` is reported as requiring authorization; a `403` that names
`insufficient_scope` in any `WWW-Authenticate` line as needing a broader scope. Every dial
to a remote server goes through an egress gate (a configured public name that resolves to a
private address is refused) and leaves a receipt in `.rapidlm/egress-receipts.jsonl`; a
connection or redirect the egress policy denies before dialling leaves a refusal receipt. A
server the managed policy blocks is never attempted, so it has nothing to receipt.

`rapid mcp install` writes a remote entry with its `type`, `url`, `headers` and the *names*
in `bearer_token_env` and `headers_env`; the last two are keys only this tool reads, so
the entry installed into another program's file carries no credential there.

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
