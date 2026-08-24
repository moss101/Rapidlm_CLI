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

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
