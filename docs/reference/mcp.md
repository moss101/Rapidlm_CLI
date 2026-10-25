# MCP servers

How a project's MCP servers are declared, how much of each server the model sees, how long each
may take, and how a large server stays out of the tool list until it is needed. The commands that
manage servers (`rapid mcp list|get|add|remove|probe|install`, `/mcp`) are in
[`cli-command-reference.md`](cli-command-reference.md#rapid-mcp--project-mcp-servers); this page is
the reference for what the settings mean.

## Declaring a server

Servers are declared under `mcpServers` in `.rapidlm/settings.json`, then in the compatibility
settings file `.claude/settings.json` (read only; the first file wins per name; at most 8 servers
in all). A server is a **stdio** entry (`command`, `args`, `env`) or a **streamable HTTP** entry
(`"type": "http"`, `url`, `headers`, `bearer_token_env`, `headers_env`). A key an entry does not
know is ignored; a key this page describes with a value it does not accept rejects the entry, with the
reason shown by `rapid mcp list`, `rapid doctor` and a warning on each turn. A rejected entry that
asks for fewer tools, a time limit or deferral still holds its name, so a plain entry of the same
name in the second file is not run in its place.

| Key | Meaning |
|---|---|
| `enabled_tools`, `disabled_tools` | which of the server's tools the model may use (below) |
| `startup_timeout_ms` (1 000–120 000), `tool_timeout_ms` (1 000–600 000) | how long the server gets to start and to answer a call (below) |
| `deferred` (`true`/`false`) | keep the server's tools out of the tool list until the model loads them (below) |

```jsonc
// .rapidlm/settings.json
{
  "mcpServers": {
    "tracker": {
      "type": "http",
      "url": "https://tracker.example/mcp",
      "enabled_tools": ["search_*", "get_*", "create_issue"],
      "disabled_tools": ["get_audit_log"],
      "deferred": true,
      "tool_timeout_ms": 60000
    }
  }
}
```

The user's `config.toml` and the managed policy add:

```toml
# ~/.rapidlm/config.toml
[mcp]
startup_timeout_ms = 30000   # defaults for every server
tool_timeout_ms    = 30000
default_deferred   = false
max_loaded_tools   = 24
```

```toml
# managed policy
[mcp]
max_tool_timeout_ms = 120000   # the longest any call may be given
```

A managed `[mcp] allowed_servers` / `denied_servers` still decides which servers may be added and
bound at all; a filter can only remove tools, so it never brings back what the policy refuses.

## Remote servers that ask for authorization

A remote server may answer a request with "authorization required" (`401`, or `403` naming a
scope). What a person sees depends on what the entry says, with the `auth` key:

| `auth` | Meaning |
|---|---|
| absent | a server that asks is waiting for the person to authorize it (below) |
| `false` | never: a `401` is a plain failure, and the entry says so |
| an object | the same as absent, with the requests a project may make of the flow: `client_id` (at most 256 printable characters), `client_secret_env` (an upper-case environment variable name), `scopes` (at most 16, each at most 64 characters, no quote or space) and `callback_port` (1024–65535). It cannot name an endpoint, an issuer, a redirect host or a resource. |

An entry that has a credential of its own — `bearer_token_env`, `headers_env`, an `Authorization`
header, or any header whose name says it is a key or a token (`X-Api-Key`, `X-Auth-Token`) — is
`static`: it never asks anyone to authorize, an `auth` object beside it is refused, and a `401` from
its server is a plain failure that says the server did not accept the credential. (The same rule
decides which headers may not travel over plain `http://`.) A token does not cross an unencrypted
network: authorization is started for an `https` URL, or `http` on this machine, only. For any other
`http://` URL an `auth` object is refused, and a `401` is a plain failure that says so (state
`disabled`). `auth` on a stdio entry means nothing and is ignored, as any key the loader does not
read is.

Every remote server is in one state, shown by `rapid mcp list` (`oauth=`) and `rapid mcp get`, and by
`rapid doctor`, **offline** — from the entry and from a small record the RapidLM home keeps of which
servers asked (`mcp-auth-state.json`, owner-only, at most 64 KiB, the oldest entries forgotten first:
a hash of the server's URL — a URL can carry a secret in its path, so it is not kept — beside its
origin, the scopes its challenge named and when; never a token, a code or a secret):

| State | Meaning |
|---|---|
| `static` | a credential of its own is configured |
| `disabled` | `auth: false`, or a plain `http://` URL off this machine |
| `not_required` | the server has not asked |
| `needs_auth` | it asked and nothing has resolved it |

A server in `needs_auth` has **no tools** in a session. One tool, `mcp__<server>__needs_auth`, stands
for it; calling it fails with "server `<name>` requires authorization; ask the user to run `rapid
mcp auth <name>`" (and, for a `403` that says the scope is not enough, which scopes it named — only
letters, digits and `. _ : / @ -` survive; for a plain `401` nothing the server chose is shown to the
model, and `rapid mcp get` shows the person the scopes). The model cannot start the authorization:
there is no tool for it, and it is not reachable from a slash command a model could send, from a hook
or from a goal. The server is connected again at the start of each turn — one attempt a turn, and a
turn resumed after an approval is the same turn — and the record is cleared the first time it
answers a call (answering the handshake is not enough: some servers answer `initialize` and
`tools/list` to anyone). A server that answered and then refused a call is put in the same state for
the rest of the turn. `rapid mcp probe` clears the record of a server that came up.
`rapid mcp probe` reports a server that asks as `failed=… reason=requires authorization (HTTP 401);
run \`rapid mcp auth <name>\`` and remembers it.

## Tool filters

A server entry may carry `enabled_tools` (an allowlist; absent means
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

## Timeouts

A server gets a limit to start (each wait while it starts, the `initialize`
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

## Deferred definitions

A server's tools cost the model a full definition each, on every
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
empty (then the first tools in name order; a query of nothing but punctuation has no token and
finds nothing), highest score first and then by name, at most 20
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
never one the same request loaded or asked for again in it. Each load and unload is a ledger record
(`mcp.tools_loaded`, `mcp.tools_unloaded`: names and schema digests, never text), written before
the next tool list is built, and the loaded set is rebuilt from the ledger when the session is
not the one it was last for — after a restart, a resume, a fork or a rewind. A ledger that
cannot be read through gives no set rather than an empty one — and not the set of the session
that was open before: the turn says that none is loaded and reads the record again at the next.
A record longer than the part that is read (the newest 65 536 events of each session, a fork
chain eight deep) is read as far as that goes, and the turn says so. A record that cannot be
written is said in the result of the load (`note: …`), since the tool would not be there after a
restart. At the start of a turn each deferred server is listed again, the whole listing within its
start-up limit (not its call limit, and not frame by frame): a loaded tool whose schema digest
changed or that the server no longer reports is unloaded (`stale`), one whose server is up but no
longer deferred is unloaded (`undeferred`), one whose server is down is unloaded
(`server_offline`), and a set that has outgrown what the cap, the provider's tool limit and the
20 KiB allow now (a lowered `max_loaded_tools`, a plain server that grew, a description that did)
is trimmed, least recently called first (`evicted`). The terminal UI shows each unload from the
ledger; a turn that cannot show a notice from the ledger (a restore that failed, an unload that
could not be written) says it as a notice. A server whose transport is gone or that does not
answer the listing in time is down for the turn — its marker shows in the manifest, and the next
registration connects it again — while one that answers with an error (busy) is left as it was.
Plain servers are listed first and the tools the model has loaded after them, so loads never take
the room of a server that is not deferred. A
tool counts as the most recently called when the model proposes it, whatever the permission
check then says, which is what the ledger's `tool.requested` gives when the set is rebuilt. In a
step that has a load in it, the calls to tools of deferred servers run in the order they were
asked beside the load, so a call asked after its load finds the tool loaded and one asked before
it does not (calls to other servers' tools still run side by side). Compacting the
conversation unloads nothing. A child does not inherit the parent's loaded set. Nothing is deferred where a load
cannot be put on record (a workflow step, a child, a run that is not recorded): such a server's
tools are listed in full, as before. With no `deferred` anywhere, nothing here exists.

## Subagents

A child has no MCP tools unless its definition names them: an `allow_tools` pattern that starts
with `mcp__` (`mcp__docs__*`, `mcp__db__query`), the `mcp` class in its `tool_classes` (a base
role that exposes it — `explorer`, `planner`, `reviewer`, `verifier`, `main`), and a tool that a
connected server lists and its own filters allow. The child uses the session's connection to the
server — one process, the same filters and limits — and is offered what its patterns select as
ordinary entries even when the server is deferred for the parent (its own loaded set is empty; it
loads nothing and the parent's is untouched). The selection must fit a tool list (20 KiB of
definitions, the provider's tool cap less the built-in tools) or the spawn is refused as
`too_large`. Calls are classified and asked as the parent's, in the child's permission context
(the parent's, narrowed for a subagent): a child has no one to ask, so a call that would ask needs an
allow rule or grant — also under a parent that bypasses every ask. An MCP tool is outside the
guarantees of the built-in tools' scopes (the read-only scope, `write_scope`, a worktree's
isolation): it does what its server does where the server runs. `rapid agents validate` warns about
a pattern that names MCP tools in a definition that does not keep the class. See
`docs/reference/agent-definitions.md`.

## What is recorded

Two ledger records, appended before the next tool list is built, names and digests only:

- `mcp.tools_loaded` — `server`, `names[]` (the server's own tool names), `schema_digests[]`
  (SHA-256 of each canonical input schema), `definition_bytes[]`, `origin` (`model`) and
  `loaded_set_size`.
- `mcp.tools_unloaded` — `server`, `names[]`, `reason` (`evicted`, `stale`, `server_offline` or
  `undeferred`).

They are in the `tool` family (see [`event-catalog.md`](event-catalog.md)). No description, schema or
argument from a server is ever written to them.

## Failures the model reads

| Text | When |
|---|---|
| `mcp_tool_not_enabled: tool \`x\` is not enabled for server \`s\`` | the settings filter the tool out (the server is not contacted) |
| `mcp_tool_not_loaded: \`mcp__s__x\` is a deferred tool; call \`mcp_load\` with its name first` | a deferred tool that has not been loaded (the server is not contacted) |
| `mcp_timeout: \`x\` did not answer within N ms (server \`s\`); the call was cancelled` | a call outlived its limit |
| `startup_timeout (the server did not finish starting within N ms)` | the reason a server is offline when it was too slow to start |
| `server \`s\` requires authorization; ask the user to run \`rapid mcp auth s\`` | the call of a remote server's `needs_auth` marker, or of any tool of a server that has started to ask (above) |

## Limits worth knowing

- A tool list of a server that is not deferred is capped at 20 KiB of MCP definitions in all, first
  registered first, and at the provider's 64 tools in all with the built-in ones; a tool the
  provider would refuse (a schema over 16 KiB, a description over 4096 bytes, a name outside what
  every provider takes — letters, digits, `-` and `_`, at most 64 bytes with the `mcp__<server>__`
  prefix, so a tool called `get.issue` is not offered — or a schema that is not an object) is left
  out of the list rather than failing the model's step; `/mcp` says how many. For a deferred
  server such a tool is not in the manifest, not found and not loadable. A schema of `null` is
  read as no parameters. Text a server sends is shown with control characters, line separators,
  direction marks and the invisible tag characters turned into spaces (the joiners that spell
  Persian, Indic scripts and emoji stay).
- A write to the pipe of a stdio server that has stopped reading cannot be interrupted; the
  cancellation notice after a timeout is best effort.
- `tools/list` is read once at connect for a server that is not deferred; a deferred server is
  listed again at the start of every turn. Pagination and list-changed notifications are not
  handled.
- The loaded set is read back from at most the newest 65 536 events of each session in a fork chain.
