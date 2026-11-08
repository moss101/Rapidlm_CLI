# EXT-02 — Remote MCP servers: a working HTTP transport and user-initiated authorization

**Status:** draft behavior spec (not yet scheduled; see [README](README.md))  
**Size:** L (five slices; slice 0 stands alone and is worth shipping first)  
**Depends on:** nothing. Slice 0 fixes defects that block every remote server today; slices 1–4 build on it.  
**Invariants touched:** core 5, 6, 10, 11, 13; SEAMS S2, S4, S5, S6, S9, S10, S11  
**Baseline:** `05ad9d6`. Every "Today" line is a reading of that tree and must be rechecked at Phase 0 (SEAMS §5.1); lines marked *(spot-checked)* were re-read by the author of this spec.

---

## 1. Intent

A growing share of useful MCP servers are hosted: they live behind an HTTPS URL and want the user to authorize RapidLM before they answer. RapidLM can *declare* such a server (`type:"http"`) but cannot reliably call one, and cannot authorize against one. This spec delivers, in order:

- **Slice 0 — the transport works.** A `tools/call` over HTTP returns its result, whether the server answers with a JSON body or an event stream, and a static credential can be supplied without writing a secret into a settings file.
- **Slices 1–4 — authorization.** When a server answers "authorization required", the user can authorize it with a short, explicit, auditable command; RapidLM then stores the resulting tokens as credential handles, attaches them at the transport boundary only, refreshes them, and can revoke them. The model can never start the flow, see a code or a token, or widen what the flow is allowed to contact.

The authorization design follows the published MCP authorization profile of OAuth 2.1: authorization code with PKCE (S256), protected-resource and authorization-server metadata discovery, optional dynamic client registration, resource indicators and refresh tokens. This spec states the *RapidLM behavior* around that profile; where the profile and this text disagree on wire details, the profile wins and the disagreement is a Phase 0 finding.

## 2. Today

- **Declaration.** `{"type":"http","url":…,"headers":{…}}` entries parse (`apps/rapid/src/mcp_config.rs:519-551`); a bare `url` or any other `type` is rejected as an unsupported remote transport (`mcp_config.rs:552-562`). Header bounds: ≤16 headers, key ≤128 bytes, value ≤4096 bytes (`mcp_config.rs:64-67, 529-551`). The URL is stored in the `command` field of the internal entry (`mcp_config.rs:550`).
- **Docs and messages say otherwise.** "Stdio only" is stated in five places — `docs/reference/cli-command-reference.md:304`, `mcp_admin.rs:162`, `mcp_config.rs:238`, `interactive.rs:2109` and `transport.rs:852` — and two tests assert the string (`mcp_admin.rs:1393`, `interactive.rs:31100`). A stale comment at `exec_tools.rs:9566-9570` says the session API "is not yet driven by the production tool-call path", which `exec_tools.rs:6533` contradicts.
- **Defect A — `tools/call` over HTTP cannot work.** `tools_call_with` sends a frame and then receives one; for HTTP, `send_frame` posts and discards the reply and `recv_frame` always returns `InvalidFrame` *(spot-checked, `crates/mcp/src/transport.rs:862-881`; callers `exec_tools.rs:6533, 9616`)*. No cargo test drives MCP over HTTP end to end; the fixture server answers `initialize` and `tools/list` only (`examples/mcp-http/fixture-server.py`).
- **Defect B — a configured `Authorization` header cannot work.** The shared request writer rejects any `Authorization` header *(spot-checked, `crates/llm-router/src/providers/openai_compatible.rs:437-442, 2498-2504`)* while always emitting its own `Authorization: Bearer <token>` on POST (`openai_compatible.rs:2460`), empty when unset. The adapter that wants to add a header (`mcp_http.rs:69`) therefore fails; `docs/gap-to-delivery.md:162` claims it works.
- **Defect C — every non-200 looks like a failed handshake.** 401, 403 and a 202 reply to a notification all map to `HandshakeFailed` (`transport.rs:829-831`), `WWW-Authenticate` is not kept (`HttpResponse` carries status, session id, `Location` and body only — `transport.rs:179-184`, `mcp_http.rs:98`) and `Accept` is `application/json` only (`mcp_http.rs:55`), so an event-stream reply cannot be requested.
- **Defect D — `rapid mcp install` writes entries this loader rejects.** It emits `url`/`headers` without `type` (`apps/rapid/src/mcp_install.rs:84-91`).
- **The credential seam exists and is unused.** `HttpAuthScope` (target `mcp.http.auth/<scheme>/<host>/<port>`) and `HttpRequest::credential()` (`transport.rs:134, 502-525, 571, 1576`); connect passes `bearer: None` (`exec_tools.rs:9450-9457`); the io adapter ignores the credential (`mcp_http.rs`).
- **Credential storage.** `CredentialStore` offers put/get/delete/list_metadata (`crates/auth/src/store.rs:135-148`); `OauthRefresh` and `OauthAccess` kinds exist (`store.rs:64-71`) with **no production caller**; persistence is the OS keychain only, blocked when unavailable, with no file fallback (`os_keychain_select.rs:19-48`; `store.rs:54-59`). `SecretBroker` issues one-use handles scoped to a target (`broker.rs:46-100`). The redaction registry accepts runtime canaries (`crates/security/src/redaction.rs:225-235`, installed through `set_redaction`, `exec_tools.rs:3378`).
- **Nothing for the interactive part exists.** No PKCE, device-code or redirect handling, no URL opener, no loopback listener outside test code; the daemon uses a Unix socket (`daemon_serve.rs:133`). `rapid browser` drives a headless Chromium over CDP and is unrelated (`crates/computer-use/src/browser/cdp.rs`). `/mcp auth <name>` parses (`crates/tui/src/commands.rs:1803`) but answers "only stdio" (`interactive.rs:2108`).
- **Egress.** MCP HTTP is allowlisted to the configured host only (`exec_tools.rs:9431-9439`), with no dial gate and **no receipt** (`mcp_http.rs:32, 43`); receipts exist for web fetch, web search, provider calls and the update check (`exec_tools.rs:6631, 6669`; `update_notice.rs:352-376`).
- **Human waits.** The approval request/sink is at `approvals.rs:45-102`; `NeedsApproval = 10` (`headless/jsonl.rs:104`). MCP elicitation (SEAM-11-1) handles stdio form schemas only (`exec_tools.rs:6500-6508`); the HTTP session does not declare elicitation (`exec_tools.rs:9472`).
- **Reusable.** `get_raw` with `with_dial_gate` and `ProviderEgress` receipts (`update_notice.rs:352-376`, `openai_compatible.rs:377`), SSRF classification in the provider HTTP client, the auth store and broker, `sha2` (`approvals.rs:60`), approval rendering.

## 3. Behavior — Part A (slice 0): the remote transport works

**A1 — Replies of either framing.** Requests carry `Accept: application/json, text/event-stream`. A reply that is a JSON body is returned as before. A reply that is an event stream is read until the event carrying the matching response id, bounded by: one frame ≤ 64 KiB (as today), the whole stream ≤ 256 KiB, and the tool's timeout (EXT-01 §3.2) as the wall limit. Events for other ids and server-initiated messages are handled as the stdio path handles them (ignored in this slice).

**A2 — Notifications.** A POST of a notification that the server accepts with an empty `202` is a success.

**A3 — Status mapping.** `401` → `Unauthorized { challenge }`, `403` with an `insufficient_scope` error → `InsufficientScope { scopes }`; other statuses as today. The `WWW-Authenticate` header is kept (≤ 2 KiB), parsed for `resource_metadata`, `scope` and `error`, and never logged unredacted.

**A4 — Static credentials.**
- A configured header named `Authorization` is accepted from `headers` and used verbatim (the shared writer must stop rejecting it for this caller).
- A new key `bearer_token_env` names an environment variable (`[A-Z_][A-Z0-9_]{0,63}`); its value becomes `Authorization: Bearer <value>` at the io boundary. The value is resolved at connect, registered with the redaction registry, and never printed by any command (invariant 11). Both forms count as **static auth**: when either is present the OAuth machinery of Part B never engages for that server.
- `rapid mcp add` is stdio-only today (`mcp_admin.rs:123-129`: no `--url`, no `--type`). It gains `--url <https-url>` for a remote entry together with `--header-env NAME=ENVVAR` and `--bearer-env ENVVAR`; secrets never appear in argv. `--url` and `--command` are mutually exclusive.

**A5 — Receipts and gate.** Every connection to a remote MCP host goes through the dial gate and appends an egress receipt (S10), under the same policy that governs provider calls. This is a behavior change for existing remote entries: they gain receipts, not new restrictions.

**A6 — Install.** `rapid mcp install` writes `type` for remote entries so its output loads (Defect D).

**A7 — Docs and messages.** The five stdio-only statements listed in §2 are corrected and the two tests that assert the old string are updated in the same change.

## 4. Behavior — Part B (slices 1–4): authorization

### 4.1 States

Each remote server has an authorization state, a projection of stored metadata and ledger events (S6):

| State | Meaning |
|---|---|
| `static` | static auth is configured (A4); OAuth never engages |
| `not_required` | the server has never answered 401 |
| `authorized` | tokens are stored and not known to be rejected |
| `needs_auth` | a 401/403 challenge arrived, a refresh failed, or no token exists yet |
| `disabled` | `auth: false` in the entry, or managed policy forbids authorization |

An entry's `auth` key may be absent (engage on challenge), `false` (never engage; a 401 is a plain failure) or an object (4.7).

### 4.2 A `needs_auth` server in a session

The server's tools are absent and the server is represented by one marker tool `mcp__<server>__needs_auth` with no parameters whose result text is: "server `<name>` requires authorization; ask the user to run `rapid mcp auth <name>`". It reuses the pattern of the existing offline marker. At most one connect attempt is made per turn (no retry storm). **The model cannot start the flow**: there is no tool, no slash dispatch from a model, hook or autonomous goal path, and no deep link (4.3).

### 4.3 The command

`rapid mcp auth <server> [--no-browser] [--dry-run] [--timeout <s>] [--yes]` and, in the terminal UI, `/mcp auth <server>` typed by the user. Both call one function. Like `rapid permissions` and `rapid mcp add`, the command is reachable only from argv or a user-typed slash command: obtaining a credential is a privileged act. In agent mode it is refused with a typed error unless `--yes` and `--non-interactive` are both given **and** the entry names a pre-registered client (so no registration side effect happens unattended).

Steps, in order, each of which can stop the command with a typed error and "nothing was stored":

1. **Gate.** The project is trusted; the server is allowed by managed policy (`allowed_servers`/`denied_servers`); managed `[mcp] allow_authorization` is not `false`; the credential store is writable (4.8).
2. **Discover.** Fetch protected-resource metadata from the challenge's `resource_metadata` URL if one was seen, else from the server origin's well-known location; choose an authorization server it lists; fetch that server's metadata. Discovery requests are `https` only, except that when the MCP server URL is itself `http` on a loopback host the discovery of *that* server may be `http` on loopback; each reply ≤ 64 KiB; ≤ 3 redirects, each re-authorized; 10 s per request; SSRF classification as for provider calls; no credential is sent.
3. **Show the plan and ask.** Before anything opens a browser or registers a client the command prints: server name and URL; issuer origin; the hosts it will contact (discovery, registration, authorization, token, revocation); scopes requested; whether dynamic registration will happen; the redirect it will listen on. On a terminal it asks `y/N`; without a terminal it requires `--yes`. `--dry-run` stops here, having performed only the read-only discovery and stored nothing.
4. **Client identity.** Use the entry's `auth.client_id` (and `client_secret_env` when named) if present. Otherwise, if the server advertises a registration endpoint, register a public client (native application, loopback redirect, no client secret) and keep the returned identifier as non-secret metadata. Otherwise stop with `no_client` and a hint to set `auth.client_id`.
5. **Authorization request.** Authorization code with PKCE `S256`, a `state` of at least 128 random bits, the canonical MCP server URL as the `resource`, and `scope` from `auth.scopes`, else from the challenge, else omitted.
6. **Redirect.** A listener bound to `127.0.0.1` only (ephemeral port, or `auth.callback_port` for pre-registered clients), path `/callback`. It accepts exactly one `GET` whose `Host` equals the bound address and whose `state` matches, carrying `code` or `error`; any other request is answered with a static page that echoes nothing and does not end the wait; five invalid requests abort. On the matching request it answers a static "you can close this window" page and closes. The wait ends at the timeout (default 300 s). With `--no-browser` the command prints the URL and *also* accepts the callback URL pasted on standard input, which makes the flow usable over SSH.
7. **Open the browser.** The platform opener is run **without a shell**, the URL as one argument; where no opener exists, or `RAPIDLM_NO_BROWSER` is set, the URL is printed instead. Never opened in a non-interactive run.
8. **Exchange.** Token request over TLS with the code, verifier, redirect URI, client id and resource. The reply is validated: `token_type` is Bearer (case-insensitive); `access_token` non-empty, ≤ 8 KiB; `expires_in` sane (≤ 30 days; absent means "until rejected"); if the redirect or the reply carries `iss` it equals the discovered issuer. An `access_denied` redirect is the typed `denied_by_user`.
9. **Store.** Access and refresh tokens are written as `OauthAccess` / `OauthRefresh` credentials through the credential store, keyed by canonical server URL, issuer and client id (decision D-5). Token values, the authorization code, the PKCE verifier and any client secret never appear in settings files, ledger events, logs, standard output or error text. Non-secret metadata (issuer, client id, scopes, expiry instant) is stored beside them.
10. **Record.** An audit record `mcp.authorized` (server, issuer, client id, scopes, expiry; never a token) is appended where `rapid trust grant` and `rapid permissions allow` append theirs, and to the session ledger when invoked from the terminal UI (decision D-2).

### 4.4 Using the tokens

- The `Authorization` header is attached at the io-adapter boundary only (the existing "configured-header authentication boundary"); the transport asks for the credential by `HttpAuthScope` target and resolves it just in time. Tokens are never held in the entry struct, the surface or the prompt.
- **Refresh.** Proactively when the access token expires within 60 s; reactively once on a 401, then the request is retried once. Refresh is **single-flight** per server across concurrent calls in a process, and a file lock covers concurrent processes. A rotated refresh token is persisted **before** the new access token is used; if persisting fails the new token is not used and the state becomes `needs_auth`.
- **Failure.** `invalid_grant` on refresh deletes the refresh token and sets `needs_auth`. A `403 insufficient_scope` sets `needs_auth` and puts the required scopes in the marker text; there is no automatic step-up.

### 4.5 Pinning

The first successful authorization pins the issuer origin for that server URL. A later discovery that yields a different issuer is refused with `issuer_changed` (showing both) unless the user re-runs the command with `--rebind` and confirms. Refresh contacts only the pinned issuer's token endpoint. Changing the server's `url` in settings changes the credential key, so tokens never follow a URL change.

### 4.6 Logout and removal

`rapid mcp logout <server> [--forget-client]` deletes the stored tokens, attempts revocation if the authorization server advertises an endpoint (egress, receipt, best effort — failure is reported, not fatal), and appends `mcp.deauthorized`. `rapid mcp remove <server>` also deletes that server's credentials and says so.

### 4.7 What a project file may say

`auth` may be `false` or an object with only: `client_id` (≤ 256 bytes), `client_secret_env` (an environment variable name), `scopes` (≤ 16 tokens, each ≤ 64 bytes of `[\x21-\x7e]` without quotes) and `callback_port` (1024–65535). It cannot set endpoints, an issuer, a redirect host or a resource. A project-supplied `client_id` is a request: it appears in the plan the user confirms (S5).

### 4.8 Where tokens live

Only in the credential store. If no secure store is available the command stops with `no_secure_store` and a hint; there is **no plaintext fallback** (decision D-1). Nothing in a settings file is ever a token.

### 4.9 Policy

Managed `[mcp]` gains `allow_authorization` (default true; `false` disables Part B entirely and `rapid doctor` says so) and `allowed_issuers` (patterns over the issuer origin; narrow-only). The flow's one-time egress allowance covers exactly the hosts the user confirmed in step 3; each contact appends a receipt; at run time only refresh to the pinned token endpoint (and revocation) are allowed. A denied server or issuer stops the command before any network contact.

### 4.10 Visibility

`rapid mcp list` gains an `auth` column (`static`, `authorized (expires in 41m)`, `needs_auth`, `n/a`); `rapid mcp get` adds issuer host, scopes and expiry. The doctor `mcp` row reports the same **offline** from stored metadata and WARNs when the credential store is unavailable. No command ever prints a token.

## 5. Records and contracts

| Surface | Change |
|---|---|
| Audit record / ledger | `mcp.authorized`, `mcp.deauthorized`, `mcp.auth_failed { reason }` — reasons: `denied_by_user`, `timeout`, `no_client`, `issuer_changed`, `no_secure_store`, `policy`, `exchange_failed` |
| Egress receipts | every discovery, registration, authorization-adjacent, token and revocation request |
| Credential store | `OauthAccess`, `OauthRefresh` entries plus non-secret metadata |
| Managed policy | `[mcp] allow_authorization`, `allowed_issuers` (closed table; unknown keys refused as today) |
| Event kinds | registered as in EXT-01 §5 |

## 6. Failure and edge behavior

- Port already in use: pick another ephemeral port unless `callback_port` is fixed, then fail with a typed error naming the port.
- Two concurrent `rapid mcp auth` for one server: the second waits on the lock up to 10 s, then reports "another authorization is in progress".
- The browser never returns: timeout, listener closed, nothing stored.
- Clock skew: expiry is computed from the receipt instant and `expires_in`, not from a server-supplied absolute time.
- Proxies: the flow uses the same `[network] proxy` configuration as provider calls.
- Windows: the loopback listener is portable; the opener is a documented per-platform argv; where none exists the URL is printed (typed unavailability, tested).
- IPv6-only loopback is not supported in this spec.
- A `401` on a `static`-auth server is a plain failure with the response status; it never starts anything.

## 7. Acceptance criteria

**Slice 0**
- **AC-01** Against a loopback stub, `tools/call` over HTTP returns the result for a JSON reply **and** for an event-stream reply (matching-id extraction, other events ignored, size bounds enforced); a `202` notification succeeds. This test did not exist before and fails on the baseline.
- **AC-02** A configured `Authorization` header is sent verbatim; `bearer_token_env` resolves at connect, is redacted in doctor output and in every error path (canary scan), and a missing variable is a typed error naming the variable, not its value.
- **AC-03** 401 and 403 are distinguished from other failures; the `WWW-Authenticate` header is kept, parsed and bounded (fixtures including oversized and malformed values).
- **AC-04** Every remote MCP connection appends an egress receipt; a host denied by policy is refused and receipted.
- **AC-05** `rapid mcp install` output loads under the project loader (round-trip test); `rapid mcp add --url … --bearer-env …` writes an entry that loads and never puts a secret in argv, a file or output; the five "stdio only" statements are gone and their two tests updated.

**Slices 1–4** (stub MCP server + stub authorization server on loopback)
- **AC-06** A 401 puts the server in `needs_auth`; the marker tool is present; exactly one connect attempt per turn; `static` servers never enter the state.
- **AC-07** Full flow with dynamic registration: the PKCE challenge matches the verifier (S256); `state` and `resource` are sent; tokens are stored through the credential store; **settings files are byte-identical before and after**; a canary scan finds no token, authorization code, verifier or client secret in standard output, standard error, the ledger, logs or any file under the project (the printed authorization URL legitimately contains the single-use `state` and the PKCE challenge).
- **AC-08** Pre-registered client (`client_id`, `client_secret_env`) works without a registration request.
- **AC-09** Callback hardening: wrong `state`, non-GET, wrong `Host` are ignored without ending the wait; the fifth invalid request aborts; timeout aborts; none of them stores anything. The listener is bound to `127.0.0.1` only (asserted).
- **AC-10** `--dry-run` performs discovery only: the stub registers no client, issues no code and the credential store is unchanged. Without a terminal and without `--yes` the command refuses with a typed error.
- **AC-11** Plan accuracy: every host the stub sees contacted is in the printed plan, and every contact has a receipt.
- **AC-12** Refresh: proactive at expiry − 60 s; reactive once on 401; a rotated token is persisted before use; N concurrent callers cause exactly one refresh at the stub; `invalid_grant` deletes the refresh token and yields `needs_auth`.
- **AC-13** `iss` mismatch, a non-Bearer `token_type`, an oversized token and a non-HTTPS discovery URL (outside the loopback exception) are each refused with a typed reason.
- **AC-14** Issuer pinning: a changed issuer is refused; `--rebind` with confirmation succeeds and is recorded.
- **AC-15** Managed `allow_authorization = false` and `allowed_issuers` refuse **before any network contact** (stub sees zero requests), reporting field, origin and remediation.
- **AC-16** `--no-browser` accepts a pasted callback URL; a fake opener binary on `PATH` records that it was invoked without a shell with the URL as a single argument.
- **AC-17** No model tool, hook, goal path or slash dispatch from a model can start the flow (negative tests on each dispatch table); agent mode refuses without `--yes --non-interactive` and a pre-registered client.
- **AC-18** `logout` deletes credentials and attempts revocation (stub sees it); `remove` deletes credentials; changing `url` leaves the old credentials unused.
- **AC-19** With the credential store unavailable the command stops with `no_secure_store` and writes nothing.
- **AC-20** The doctor `mcp` row and `mcp list|get` show state, issuer host, scopes and expiry offline, never a token.
- **AC-21** Revert cycle (S8) for every AC; Windows runs the listener and callback tests and a typed-unavailability test for the opener.

## 8. Slices

0. **Transport works** — Part A, AC-01…05. Independent of the rest and useful alone.
1. **Challenge surfaced** — status mapping consumed, `needs_auth` state, marker tool, list/get/doctor columns. (AC-06, AC-20 partly)
2. **The flow** — discovery, registration, PKCE, listener, opener, exchange, storage, plan and confirmation. (AC-07…11, 13, 16, 17, 19)
3. **Refresh and revocation** — single-flight refresh, rotation, logout, remove. (AC-12, 18)
4. **Policy and pinning** — managed keys, issuer pinning, receipts for the whole flow, docs. (AC-14, 15, 20, 21)

## 9. Out of scope

- Device-code and client-credentials grants; mutual-TLS client authentication; enterprise token-exchange profiles.
- Automatic scope step-up; authorizing stdio servers.
- Making authorization a durable graph wait (decision D-3).
- MCP elicitation over HTTP and server-initiated messages on the event stream.
- A plaintext token file under any condition.

## 10. Decisions needed

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D-1 | Credential store unavailable | (a) refuse (b) in-memory for the running terminal UI only | (a). A later `--session-only` can be specified once someone needs it. |
| D-2 | Where `rapid mcp auth` records its audit event outside a session | the file `rapid trust grant` writes, a new project journal, the operation journal | Phase 0 identifies where `trust grant` and `permissions allow` record; reuse it. |
| D-3 | Authorization as a durable wait | v1: marker tool + separate command; v2: an approval with source `mcp-auth:<server>` that parks the turn (`NeedsApproval`) so the user can authorize in another terminal and resume | v1 now; v2 only after EXT-02 lands, reusing the elicitation path. |
| D-4 | Agent mode | refuse always; allow with `--yes --non-interactive` for pre-registered clients | allow in that one case, as specified. |
| D-5 | Credential scope | per project; per user keyed by canonical URL + issuer + client id | per user: re-authorizing the same server in every project is hostile, and the key binds tokens to one URL and one issuer. A trusted project can already call that server, so no new reach is created. |

## 11. Contract surfaces to update

`docs/reference/cli-command-reference.md` (MCP section, `mcp auth|logout`), a new `docs/reference/mcp.md` shared with EXT-01, `docs/reference/event-catalog.md`, `docs/architecture/auth-credentials-identity.md`, `docs/architecture/mcp-acp-sdk-external-agents.md`, `docs/gap-to-delivery.md:162` (correct the claim), the managed-policy reference, `rapid doctor` documentation, and `crates/tui` for the `/mcp` panel. The `rapid mcp install` fix needs a regression fixture beside `mcp_install.rs`.
