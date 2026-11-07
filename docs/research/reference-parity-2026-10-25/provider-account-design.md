# Provider accounts (REF-011, pending)

This is the next implementation scope, not delivered account functionality. REF010
implemented provider wire protocols and source/inference token separation; it did not
implement account login. Do not treat existing MCP auth or a configured token helper
as provider-account OAuth parity.

## Exact source evidence to continue

- Pinned OpenCode fork plugin/index.ts selects exactly opencode-copilot-auth@0.0.9
  and opencode-anthropic-auth@0.0.5 by default. There is no plugin/codex.ts in this
  requested snapshot; do not analyze a different OpenCode release as this reference.
- Both packages were fetched as static npm archives with registry SHA512 integrity
  verified, no install/execution. Provenance files record exact versions/source hashes.
  Extracted sources: /tmp/rapid-reference-sdk-sources/{copilot-auth-0.0.9,
  anthropic-auth-0.0.5}/package/index.mjs.
- Copilot uses GitHub device authorization and token polling, then a separate expiring
  inference-token exchange. Public/enterprise origin bindings are distinct. The pinned
  plugin borrows VS Code product/client identities; RapidLM must use explicit product
  registration/configuration, never silently copy those identities.
- Anthropic plugin uses authorization-code PKCE, authorize host selected between
  claude.ai and console.anthropic.com, a console callback URL, and console token
  exchange/refresh. Its model fetch adds OAuth beta/bearer headers. An alternate
  console path can create an API key: that is a separate credential mutation, not
  ordinary login/refresh, and must not be started implicitly.
- Kimi's actual TypeScript repo has packages/oauth/src/{oauth.ts,oauth-manager.ts,
  token-state.ts,oauth-token-transaction.ts,storage.ts,provider-credential.ts} plus
  regional/custom registry and model refresh code. Its default auth host is
  auth.kimi.com, with device authorization at /api/oauth/device_authorization,
  device/refresh token grants at /api/oauth/token and form-encoded bodies.
  verification_uri_complete is required; pending/expired/denied are separate states.
  Refresh manager uses lifetime-aware thresholds, explicit token state and locking.
  Analyze its rotation/revoke transaction behavior before implementing persistence.

## RapidLM primitives available

MCP OAuth modules already provide bounded URL/token parsing, PKCE, callback/listener,
redacted secrets, platform opener, network exchange and keychain integration. Reuse
protocol primitives where appropriate, but keep provider account namespace, issuer,
resource origins, client registration and managed permissions separate from MCP.
provider_keychain.rs has a shared test backend and rollback-safe handle operations.
crates/auth contains typed credentials/keychain/store metadata. Provider credentials
already have lazy wire-boundary materialization and bounded process execution.

## Acceptance to implement

Provider-account browser/device flows where supported; durable keychain handles and
account metadata; issuer/origin/client binding; bounded and cancelable polling; token
rotation with restart/concurrent refresh protection; explicit revocation/tombstones;
managed refusal; native provider auth selection (including Anthropic OAuth header
semantics); CLI/terminal integration; tests that exercise real persistence/refresh and
error/cancel boundaries through local services. User-visible login must require an
explicit login command. No unsolicited account discovery, browser launch, credential
import, external OAuth registration, API-key creation or account login during this goal.
