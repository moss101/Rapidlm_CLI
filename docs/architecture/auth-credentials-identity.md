# Architecture — Authentication, Identity, Secrets and Ephemeral Credentials

## 1. Responsibility
Own user/provider/worker/plugin identities, secret handles and short-lived executor-bound credential issuance.

## 2. Non-negotiable design rules
- Models see secret handles, not plaintext, whenever possible.
- Credential cache keys include target environment/identity/generation.
- Handoff never transfers reusable capability leases/plaintext secrets.

## 3. Components
- **IdentityService** — local/user/org principals
- **SecretStore** — OS keychain/encrypted store
- **CredentialBroker** — short-lived STS/install tokens
- **WorkerPKI** — mTLS identities
- **RedactionService** — logs/events

## 4. Canonical contracts
`Principal`, `SecretHandle`, `CredentialRequest`, `EphemeralCredential`, `CredentialScope`.

## 5. Failure and recovery
Expired/revoked credentials trigger safe reissue or block; cache mismatch never reuses credentials across environments. Recovery restores handles, not plaintext.

## 6. Security and trust
Credential resolution requires capability lease and exact target; audit records use metadata/digests, not secret values.

## 7. Implementation notes
Support provider OAuth/API keys, GitHub installation tokens and cloud STS-style adapters behind one broker interface.

MCP server tokens (EXT-02, [ADR 0027](../adrs/0027-remote-mcp-authorization-storage-and-pinning.md)): OAuth access and refresh tokens go to the operating system's credential store only (both or neither), under a key that hashes the server's canonical URL, the issuer and the client id; non-secret metadata is a separate owner-only file; with no store the flow stops, there is no plaintext fallback. The first authorization pins the issuer, a change needs `--rebind` agreed to by a person at a terminal, and the managed policy can forbid the feature or confine it to named issuers.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
