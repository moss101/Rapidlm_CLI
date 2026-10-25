# ADR 0027 — Remote MCP authorization: where the tokens live, who may start it, and what pins an issuer

**Status:** Accepted for `EXT-02` (slices 1–4 built)  
**Date:** 2026-10-25  
**Builds on:** `docs/goals/ext-surface/EXT-02-mcp-remote-authorization.md` (§4.5, §4.8, §4.9, decisions D-1, D-4, D-5), the credential store behind `provider_keychain`, the egress gate and its receipts; invariants 4, 5, 8; `SEAMS §3.2 S1, S4, S5`

## Context

- A remote MCP server that answers `401` could only be used with a static credential in settings or the environment. Nothing could obtain a token for the person, and nothing bounded what an organisation allowed to do so.
- Authorization is a privileged act: it opens a browser, listens on a loopback port, registers a client at a third party and stores a credential that later requests carry. Every one of those is something a model, a hook or a project file must not be able to cause.
- A token is worth most to whoever can redirect where it is sent, so the binding between a token, the server it is for and the authorization server that issued it is the security property, not an implementation detail.

## Decision

1. **Tokens live in the operating system's credential store and nowhere else.** Both tokens of a grant are stored in one operation (both or neither; if the second write fails the first is put back). Beside them `mcp-oauth.json` in the home (owner-only, bounded) holds what is not secret: issuer, client id, scopes, endpoints, times. With no credential store the command stops with `no_secure_store`; there is no file fallback and no in-memory fallback (D-1). `rapid doctor` says so.
2. **The credential key binds the grant.** Items are named by a hash of the server's canonical URL, the issuer and the client id. A changed URL, issuer or client is a different key, so a token cannot follow any of them. Credentials are per user, not per project (D-5).
3. **Only a person, from the command line, starts it.** `rapid mcp auth` and `/mcp auth` typed in the terminal UI call one function. No tool, hook, goal or model-reachable slash dispatch can; a source scan keeps it so. Agent mode needs `--yes --non-interactive` and a pre-registered client (D-4). A project file may name a client, scopes and a callback port, and nothing else: it is a request that the plan shows (S5).
4. **The plan comes before the effect.** Discovery is read-only and sends no credential; the plan names every host that will be contacted and why; the person confirms; only then is a client registered, a listener bound or a browser opened. `--dry-run` stops at the plan.
5. **The issuer is pinned by the first successful authorization.** The record for a server URL keeps the issuer identifier it was authorized with. A later discovery that names another is refused as `issuer_changed`, showing both, **before the new issuer is contacted**: no registration, no browser, no token request. `logout` keeps the pin; `logout --forget-client` removes it. Refresh contacts only the token endpoint recorded at authorization.
6. **A change is agreed to by a person at a terminal, however the rest was passed.** `--rebind` is accepted only where there is a terminal; without one the command refuses. The question ("Stop trusting … and authorize with … instead?") is asked even with `--yes`, because `--yes` answers the plan and a change of authorization server is a different question. The plan shows what is replaced; the old tokens are deleted when the new grant is stored; the audit line carries `rebound_from`. A dry run needs none of this and stores nothing.
7. **The organisation's word comes first and fails closed.** `[mcp] allow_authorization = false` and `[mcp] allowed_issuers` (patterns over the issuer origin, narrow-only) are checked before any request. A policy that exists and cannot be read refuses. The policy also governs what is already stored: tokens whose issuer it no longer admits stay in the store and are not sent, the server is `disabled` in reports, and it does not ask the person to authorize (a command that would be refused). `rapid mcp get` and `rapid doctor` name the field, its origin and the remedy.
8. **Pinning is by issuer identifier, not by origin.** The specification pins the issuer origin; this pins the full identifier (the metadata's `issuer`, without a trailing slash), which is finer: two tenants on one host (`https://login.example/tenant-a`, `/tenant-b`) are different issuers. The policy still matches patterns over the origin, as specified.

## What is deliberately not done

- No plaintext token file under any condition; no session-only mode (a later flag, if someone needs it).
- No automatic step-up of scopes; no device-code or client-credentials grants.
- Authorization is not a durable wait in a turn (D-3): a server that needs it offers a marker tool whose failure tells the model to ask the person to run the command.
- The audit line for `logout` and a failed `auth` goes to `mcp-audit.jsonl`; only `/mcp auth` in a session writes the ledger (`mcp.authorized`, `mcp.auth_failed`). `mcp.deauthorized` is not a ledger kind because nothing in a session emits it.

## Consequences

- `docs/reference/mcp.md` documents the flow, renewal, logout, the two managed keys, the pin and `--rebind`; `docs/reference/cli-command-reference.md` lists the flag and the keys.
- A project that relied on a token surviving a change of the server's URL or issuer must authorize again; that is the point.
- An organisation can turn the feature off or confine it to named issuers without editing any project, and a broken policy file turns it off rather than leaving it open.
- A server whose authorization server moved (a legitimate migration) needs one `rapid mcp auth NAME --rebind` at a terminal per user; it cannot be done unattended.
