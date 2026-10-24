# EXT-01 — MCP tool-surface control: per-server tool filters, timeouts and deferred definitions

**Status:** draft behavior spec (not yet scheduled; see [README](README.md))  
**Size:** M (four slices)  
**Depends on:** nothing. Slice 1 is a prerequisite of [EXT-05](EXT-05-agent-definitions-markdown.md) slice 5.  
**Invariants touched:** core 4, 5, 9, 10, 13; SEAMS S1, S3, S4, S5, S6, S9, S11  
**Baseline:** `05ad9d6`. Every "Today" line is a reading of that tree and must be rechecked at Phase 0 (SEAMS §5.1); lines marked *(spot-checked)* were re-read by the author of this spec.

---

## 1. Intent

An MCP server can hand the model dozens of tool definitions, and today every one of them is sent on every model step. This spec gives a project three controls over what a server contributes, none of which changes what a user who configures nothing sees:

1. **Filters** — expose only some of a server's tools.
2. **Timeouts** — per-server limits for start-up and for a call, with a managed ceiling.
3. **Deferred definitions** — a server's tools stay out of the per-step tool list until the model asks for the ones it needs, so a large server costs a one-line manifest rather than a full schema per tool per step.

Deferral serves the product thesis (verified task success per token) and is the tool-surface form of invariant "progressive capability projection": the model sees the smallest surface that lets it make the next decision.

## 2. Today

- **Config.** Servers are declared under `mcpServers` in `.rapidlm/settings.json` and then in a second, compatibility-only project settings file; the first file wins per name, and the merged set is capped at 8 (`apps/rapid/src/mcp_config.rs:47-54, 395-459`; file list `interactive.rs:2496`). Accepted keys are `command`, `args`, `env` (stdio) and `type:"http"`, `url`, `headers` (remote) (`mcp_config.rs:519-624`). Unknown keys are ignored silently. **There are no timeout, enabled or filter keys.** Names: ≤32 bytes, `[A-Za-z0-9._:-]`, no `__` (`mcp_config.rs:636-659`).
- **Lifecycle.** Servers register once per session in a shared `McpRegistry` (`exec_tools.rs:9202, 3276`); `tools/list` is fetched once and cached in `McpRegistry.surface` (`exec_tools.rs:9204`). There is no `tools/list` pagination (`crates/mcp/src/transport.rs:1235`) and no list-changed handling. A reply over the per-session frame bound — 64 KiB for stdio and for HTTP, set at `exec_tools.rs:9336, 9440` (the transport's own ceiling is 256 KiB, `transport.rs:36`) — marks the server offline (`transport.rs:1647-1682`), and an offline server is represented by one marker tool `mcp__<server>__offline` (`exec_tools.rs:3332-3346`). Servers removed from settings are never pruned from a live session (`exec_tools.rs:3276-3360`).
- **Surface assembly.** The tool list is rebuilt on **every model step**: `turn.rs:1530` → `WorkspaceTools::tool_surface` (`exec_tools.rs:10619`) → `full_surface_impl` (`exec_tools.rs:10938`, MCP block `11207-11250`). Each MCP entry is `[MCP] <description>` plus a clone of the full input schema. A cumulative 20 KiB cap applies to MCP definitions, first registered wins (`exec_tools.rs:117, 11234`). The provider boundary separately enforces ≤64 tools, ≤4096 bytes per description and ≤16 KiB per schema, and one breach fails the whole step (`crates/llm-router/src/provider.rs:72-81, 1318-1327, 1360`; `model.rs:1070-1076, 1209`).
- **Dispatch.** A call routes on the `mcp__` prefix (`exec_tools.rs:4470` → `execute_mcp_tool`, `6374`) and **does not check that the tool is in the surface**: the fallback is `rest.split_once("__")` *(spot-checked, `exec_tools.rs:6404-6408`)*. A tool omitted from the surface by the 20 KiB cap is therefore still callable.
- **Classification.** An MCP call is `ToolClass::Other`, kind Write, with its own `solo:` group (`exec_tools.rs:7347-7361, 7106-7118`). Permission rules and grants name the bare wire name; `*` and `:` are illegal in rule identifiers (`permissions.rs:130-151, 270-283`). A managed `denied_tools` can ban one exact wire name (`managed_config.rs:450-470`).
- **Timeouts.** A fixed 30 s watchdog thread that is never stopped early (`exec_tools.rs:6468-6482`), plus 30 s per frame (`exec_tools.rs:9336`). Arguments are capped at 8 KiB and are not validated against the schema (`exec_tools.rs:128, 7877`).
- **Unused building blocks.** `crates/mcp` has a paged, hashed tool catalog (`catalog.rs:26-47, 572`) and `capability_broker::projection` works at capability-family level; neither is reached from `apps/rapid`.
- **Adjacent defect, outside this spec (see README R-3).** An MCP success is summarised through `bounded_detail`, whose cap is `MAX_RESULT_DETAIL_BYTES = 256` *(spot-checked, `exec_tools.rs:220, 6586-6596, 7882`)*; `structuredContent` and images are dropped (`transport.rs:1339-1362`); the text is prefixed `[mcp:<server>]` but not fenced as untrusted. If confirmed at Phase 0 this should be fixed before or with slice 3, because a smaller tool list helps little when results are cut to 256 bytes.

## 3. Behavior

### 3.1 Filters

A server entry may carry:

- `enabled_tools` — an allowlist of patterns. Absent means "all tools".
- `disabled_tools` — a denylist of patterns, applied after `enabled_tools`.

A pattern is 1–128 bytes drawn from `A-Z a-z 0-9 . _ : - * ?`, matched against the server's **own tool name** (not the composed `mcp__server__tool` name). `*` matches any run of characters (including none), `?` exactly one; everything else is literal; matching is case-sensitive and anchored to the whole name. At most 64 patterns per list.

The **effective set** of a server is `(enabled_tools, else all) − disabled_tools`.

1. Tools outside the effective set are removed when `tools/list` arrives: they are never cached in the surface, never offered to the model, never counted toward the 20 KiB cap or the provider's tool ceiling.
2. **Dispatch checks the effective set.** A call to a tool of that server that is not in the effective set returns a typed failure `mcp_tool_not_enabled` ("tool `<name>` is not enabled for server `<server>`") and **does not contact the server**. This closes the existing gap that a name the model was never offered is still callable. A tool that is in the effective set but omitted by the 20 KiB cap stays callable exactly as today (S11); deferral (3.3) is the supported remedy for large servers.
3. A pattern that matches no tool the server reports is not an error; `rapid mcp get` and `rapid mcp probe` list it as `matches nothing` so a typo (`creat_issue`) is visible.
4. Filters are **requests** (SEAMS S5). They can only remove tools. Managed `[mcp] denied_servers`, managed `denied_tools` and project trust are checked first and cannot be overridden by `enabled_tools`.

### 3.2 Timeouts

A server entry may carry `startup_timeout_ms` (1 000–120 000) and `tool_timeout_ms` (1 000–600 000). A `[mcp]` table in user `config.toml` may carry the same two keys as defaults for every server and the environment may carry `RAPIDLM_MCP_STARTUP_TIMEOUT_MS` / `RAPIDLM_MCP_TOOL_TIMEOUT_MS`.

- **Built-in defaults equal today's behavior:** the current handshake bound for start-up (Phase 0 records the value) and 30 000 ms for a call. *Implementation note (EXT-01-2):* the recorded value is 30 000 ms, once for `initialize` and again for `tools/list`; each of the two steps keeps getting the whole start-up limit, so the limit is not a total.
- **Precedence follows SEAMS S11:** managed ceiling > environment > user `[mcp]` > per-server key in project settings > built-in. (A project's per-server value is the *lowest* configured layer; a user who needs a longer limit for a project's server sets it in their own `config.toml`.)
- A managed `[mcp] max_tool_timeout_ms` is a ceiling: a larger resolved value is lowered to it and the gate reports field, origin and remediation like the existing managed gates.
- When a call exceeds its limit the model reads a typed failure `mcp_timeout` ("`<tool>` did not answer within `<n>` ms"); the host sends a best-effort cancellation notification for that request; the call's watchdog is stopped when the call returns, so **no timer outlives its call** (today's watchdog does).
- **The per-frame I/O bound follows the resolved limit.** Today every frame has a fixed 30 s bound (`exec_tools.rs:9336`), so a `tool_timeout_ms` above 30 s could never take effect. For a call, the frame bound is the call's resolved timeout; for the handshake, the resolved start-up timeout.
- A start-up timeout leaves the server in the existing offline state with the reason `startup_timeout`. *Implementation note (EXT-01-2):* this holds over stdio and over HTTP; the router's HTTP client has no timeout error, so an exchange that fails only after using its whole allowance is read as one.
- **Timer lifetime must be observable.** The call watchdog is a cancellable handle (not a detached thread, as in `exec_tools.rs:6472`) whose cancellation and exit a test can observe.

### 3.3 Deferred definitions

A server entry may carry `deferred: true`; `[mcp] default_deferred` in user `config.toml` sets the default for servers that do not say (built-in default `false`).

For a deferred server the tools in its effective set are **not** part of the per-step tool list. Instead:

1. **Manifest.** The compiled context gains a block, fenced as untrusted with locator `mcp-manifest:<server>`, listing one line per tool: the composed wire name, an em dash, and the first line of its description cut to 100 bytes. A server contributes at most 4 KiB and all deferred servers together at most 8 KiB; tools that do not fit are summarised as `+N more — use mcp_find`. The text comes from an external server, so it is sanitized and fenced with the existing neutralising helper (`model::fence_untrusted`, `model.rs:1402`) — an embedded closing fence or a `DATA_URL:` line is rewritten (S4). That helper does not touch control characters, so this spec adds a sanitizer for manifest and finder text: every control character other than newline and tab is replaced by a space before fencing.
2. **Two built-in tools**, present only while at least one connected deferred server exists:
   - `mcp_find { query, server?, limit? }` — searches the **effective** tools of deferred servers by name and description (case-insensitive token match; no model call, no network); `limit` ≤ 20; a query is split into lowercase alphanumeric tokens and the **score** of a tool is `3 × (query tokens found in its name) + 1 × (query tokens found in its description)`; tools scoring 0 are excluded unless the query is empty; results are ordered by score descending, then wire name ascending; each result is the wire name plus a description cut to 200 bytes.
   - `mcp_load { names: [wire name, …] }` (≤ 16 names per call) — adds the named tools to the session's **loaded set**. Each name gets its own typed result: `loaded`, `already_loaded`, `unknown`, `not_enabled` (filtered out), `too_large` (would break the 20 KiB cap or a provider limit — refused instead of silently omitted), or `evicted:<name>` notices for tools unloaded to make room. The result text lists parameter names only; the schemas themselves are delivered through the tool list (below), not duplicated in the result.
3. **Loaded tools join the tool list from the next model step of the same turn** — the loop already rebuilds the surface every step — with their full definitions, exactly as a non-deferred server's tools appear.
4. **Calling a deferred tool that is not loaded** returns the typed failure `mcp_tool_not_loaded` ("call `mcp_load` with this name first") and does not contact the server. The model is never auto-loaded: determinism over convenience.
5. **Loaded-set bounds.** `max_loaded_tools` (`[mcp]`, default 24) bounds the loaded set. It is validated at load against the provider's tool ceiling minus the built-in tool count minus two (the finder tools) and lowered with a doctor warning if larger. When a load would exceed it, the least-recently-*called* loaded tool (ties: least recently loaded) is unloaded and reported in the `mcp_load` result.
6. **Staleness (invariant 9).** The loaded set records the SHA-256 of each tool's canonical input schema at load time. The registry is re-synchronised at the start of each turn; a loaded tool whose schema digest changed, or that the server no longer reports, is unloaded with reason `stale`, the manifest is regenerated, and the transcript gets one line `[mcp] <tool> changed on the server and was unloaded`. A later call returns `mcp_tool_not_loaded`.
7. **Server offline** unloads its tools (reason `server_offline`) and the manifest shows the existing offline marker for that server.
8. **Compaction does not unload.** The loaded set is tool-list state, not transcript; the manifest is regenerated from current state each step.
9. **Fork, rewind, resume.** The loaded set is a projection of the ledger (S6): a fork starts with the set as of the fork point, a rewind truncates it with the ledger, a resume rebuilds it. No in-memory structure is the only record.
10. **Permissions.** `mcp_find` and `mcp_load` are read-only, session-local, allowed in every permission mode including plan mode, and never prompt. Loading changes nothing about the *later* call: a loaded tool is still `ToolClass::Other`, still asks or denies by mode and rules, still subject to hooks. Loading can request nothing (invariant 13).
11. **Subagents.** A child does not inherit the parent's loaded set (invariant 12). Whether children see MCP tools at all is decided in EXT-05.

*Implementation notes (EXT-01-3).* (a) The two ledger kinds are filed under the `tool` family; no `mcp` family is added. (b) `origin` is `model`: a restored set is a projection of the ledger, not a re-recording, so nothing is written on restore. The `mcp.tools_loaded` record also carries `definition_bytes[]` (sizes, not text) so a replay can hold the set to its byte budget. (c) A token is "found" in a name or description when the lowercased text contains it; "name" is the server's own tool name; a repeated query token counts once; `limit` defaults to 10. (d) The manifest is rebuilt at the start of each turn, which is when it can change. (e) It enters the context as `retrieved` (untrusted) blocks, so the request builder fences it with the existing helper, and is scored above retrieved files. (f) Nothing is deferred where a load cannot be recorded. (g) A tool whose definition the provider would refuse (schema over 16 KiB, description over 4096 bytes) is `too_large` once deferred and is left out of the list, rather than failing the whole step, for a server that is not.

### 3.4 Visibility to the user

- `rapid mcp get <server>` prints the effective tool count out of the reported count (`12 of 40 tools enabled`), the deferred flag, the resolved timeouts with the layer each came from, and any pattern that matches nothing.
- `rapid mcp probe`, `rapid mcp list` and the doctor `mcp` row print per server the number of tools and an **estimated** definition size (`bytes / 4`, labelled *estimated*, S9), so a user can decide which servers to defer. Doctor stays offline and reports "not measured" for servers it did not start.
- `/mcp` in the terminal UI shows the same plus, per session, the loaded tools.

## 4. Configuration surface

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

```toml
# ~/.rapidlm/config.toml
[mcp]
default_deferred   = false
max_loaded_tools   = 24
startup_timeout_ms = 30000
tool_timeout_ms    = 30000
```

```toml
# managed policy
[mcp]
max_tool_timeout_ms = 120000
```

`rapid mcp add` gains `--enabled-tool`, `--disabled-tool` (repeatable), `--deferred` and `--tool-timeout-ms` (and, *EXT-01-2*, `--startup-timeout-ms`). An unknown key in a server entry stays ignored (S11), but a key from this spec with an invalid value rejects the entry with a reason, exactly like the existing entry validators, and the rejection appears in `rapid mcp list`, the doctor row and the per-turn warning.

## 5. Records and contracts

| Surface | Change |
|---|---|
| Ledger event `mcp.tools_loaded` | `{ server, names[], schema_digests[], origin: "model"\|"restore", loaded_set_size }` — appended **before** the next surface is built (core invariant 4). |
| Ledger event `mcp.tools_unloaded` | `{ server, names[], reason: "evicted"\|"stale"\|"server_offline" }` |
| Typed tool failures | `mcp_tool_not_enabled`, `mcp_tool_not_loaded`, `mcp_timeout` |
| Event kinds | New kinds go through `define_event_kinds!` (`crates/event-ledger/src/event.rs`), `sdk/typescript/schemas/wire.v1.json`, SDK regeneration and `WIRE_SCHEMA_SHA256` (`daemon_serve.rs`); the delivery record names every edit. `EventFamily` (`event.rs:100-127`) has no `Mcp` variant, so Phase 0 decides between adding one and filing the kinds under `Tool`. |
| Managed policy | `[mcp] max_tool_timeout_ms` added to the closed managed `[mcp]` table (`managed_config.rs:967` rejects unknown keys) with its gate and report. |
| JSONL (`rapid exec`) | the two events appear in the stream in the shape the stream already uses for ledger-derived records; nothing is added when the feature is unused (S11). |

No token, argument value or description text from a server enters a ledger event: names and digests only.

## 6. Failure and edge behavior

- A tool name in `mcp_load` containing characters outside the tool-name alphabet is `unknown`, never echoed unbounded (≤64 bytes in the reply).
- Two deferred servers exposing the same bare tool name stay distinct: wire names carry the server.
- A server that reports zero effective tools is shown in the manifest as `(no enabled tools)`; its start-up still counts toward the 8-server cap.
- `mcp_find` with an empty query lists the first `limit` tools in wire-name order — a browse, not an error.
- A schema larger than 16 KiB or a description larger than 4096 bytes makes that one tool `too_large` rather than failing the whole model step as it can today.
- Cancellation: an interrupt during `mcp_load` leaves the loaded set unchanged; the event is written only when the set changed.
- Headless `rapid exec` behaves identically; nothing in this spec needs a human.

## 7. Acceptance criteria

- **AC-01** With none of the new keys set, the tool list, dispatch, timeouts and every existing MCP test are unchanged; a managed policy without `[mcp] max_tool_timeout_ms` behaves as before.
- **AC-02** `enabled_tools` / `disabled_tools` fixtures cover `*`, `?`, literal, anchoring, case sensitivity, the 64-pattern and 128-byte bounds and invalid characters; a filtered tool is absent from the surface and a call to it returns `mcp_tool_not_enabled` while the stub server records **zero** `tools/call` requests.
- **AC-03** A filter cannot re-enable a tool banned by managed `denied_tools` or a server banned by `denied_servers` (gate test with field/origin/remediation).
- **AC-04** An invalid filter, timeout or `deferred` value rejects the entry with a reason that appears in `rapid mcp list`, the doctor row and the turn warning.
- **AC-05** `tool_timeout_ms` fails a slow stub call at the limit with `mcp_timeout` and sends the cancellation notification; a 60 s limit really allows a 45 s call (the frame bound follows the limit); the call's timer handle is observably cancelled and exited when the call returns (test hook). `startup_timeout_ms` fails a slow stub handshake with the offline reason `startup_timeout`. Precedence is tested layer by layer; a managed ceiling lowers a larger value and is reported.
- **AC-06** A deferred server's tools are absent from the step request's tool list; `mcp_find`, `mcp_load` and the fenced manifest are present; with no deferred server connected none of the three exists.
- **AC-07** `mcp_load` makes the named tools appear in the next step's tool list **within the same turn**; unknown, filtered and oversize names get their own typed result without aborting the rest of the batch.
- **AC-08** Calling a deferred tool that is not loaded returns `mcp_tool_not_loaded` and the stub records zero `tools/call` requests.
- **AC-09** Kill and restart (and a fork at an earlier seq, and a rewind) rebuild the loaded set from the ledger alone.
- **AC-10** A changed schema digest or a vanished tool unloads with reason `stale`, writes `mcp.tools_unloaded`, regenerates the manifest and adds the transcript line.
- **AC-11** A property test over random load/call/evict sequences shows the tool list never exceeds the provider ceiling and the loaded set never exceeds `max_loaded_tools`; eviction order is least-recently-called.
- **AC-12** A manifest or finder description containing a closing fence or a `DATA_URL:` line is neutralised and a control character is replaced by a space (fixtures); the per-server and total manifest bounds hold.
- **AC-13** `mcp_find` scoring and ordering match the formula in §3.3 on a fixed corpus, are deterministic and bounded; plan mode permits both finder tools and still asks or denies the loaded tool's call by the normal rules.
- **AC-14** `rapid mcp get|list|probe` and the offline doctor row show effective counts, resolved timeouts with their source layer and the *estimated* definition size; doctor starts no server.
- **AC-15** Revert cycle (S8): each AC's test fails with the change reverted for the stated reason; named in the delivery record. Windows runs the same tests against the stub server.

## 8. Slices

1. **Filters** — keys, validation, effective set, dispatch gate, `mcp get|list|probe|add` display and flags, doctor. (AC-01…04)
2. **Timeouts** — keys, `[mcp]` table, env, managed ceiling, timer lifetime fix, cancellation notice. (AC-05)
3. **Deferred definitions** — manifest, finder tools, loaded set, ledger events, event-kind plumbing, staleness. (AC-06…13)
4. **Visibility and docs** — estimated sizes, `/mcp`, reference pages. (AC-14)

## 9. Out of scope

- `tools/list` pagination, list-changed notifications and raising the 64 KiB frame cap (decision D-1).
- Changing the 8-server cap.
- MCP result handling (README R-3) and tool-name globs in the permission-rule grammar.
- MCP tools for subagents (EXT-05).
- A user-level MCP server file (all servers still come from project settings).

## 10. Decisions needed

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D-1 | Large `tools/list` replies | (a) leave the 64 KiB frame cap and no pagination; (b) follow `nextCursor` up to a bounded page and byte count for **deferred** servers only; (c) for all servers | (b): deferral is only useful for big servers, and big servers are the ones that paginate. Needs its own AC set; keep it out of slice 3 until chosen. |
| D-2 | Timeout precedence | S11 order as written, or let a project's per-server key outrank the user's `[mcp]` default | S11 order; SEAMS says the user outranks the workspace. |
| D-3 | Finder tool names | `mcp_find`/`mcp_load` vs namespaced | Keep the short names; they are built-ins and a built-in beats an MCP tool of the same name (`exec_tools.rs:11220`). |

## 11. Contract surfaces to update

`docs/reference/cli-command-reference.md` (the MCP section, which still says "stdio only" in places), a new `docs/reference/mcp.md`, `docs/reference/event-catalog.md`, `sdk/typescript/schemas/wire.v1.json` and the SDK, `docs/reference/model-configuration.md` (for `[mcp]`), `docs/reference/hooks.md` only if hooks are given MCP subjects. Commit messages and docs follow SEAMS §2.1.
