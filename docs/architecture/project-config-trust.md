# Architecture — Project Configuration, Trust and Multi-Repo Scope

## 1. Responsibility
Load configuration/rules/workspace manifests without executing project-controlled content until trust is established.

## 2. Non-negotiable design rules
- Parsing configuration is not execution.
- Project config can narrow but not broaden higher policy.
- Multi-repo scope is explicit on context/workspace/capability requests.

## 3. Components
- **ProjectRegistry** — workspace identities/roots
- **ConfigLoader** — layered typed config
- **TrustService** — untrusted/trusted decisions
- **MultiRepoScope** — repo aliases/relations
- **Watcher** — config generation invalidation

## 4. Canonical contracts
`ProjectId`, `RepoRef`, `ProjectConfig`, `TrustState`, `ConfigGeneration`.

## 5. Failure and recovery
Malformed config reports typed diagnostics and uses safe defaults; trust revocation deactivates project extensions and invalidates derived resource/tool projections.

## 6. Security and trust
Realpath/symlink containment; executable hooks/MCP/plugins stay inactive before trust; trust decisions auditable.

## 7. Implementation notes
Config precedence is deterministic. Changes increment generation so caches/policy/context can invalidate safely.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
