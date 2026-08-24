# Architecture — Security Scanners and Secure Coding Gates

## 1. Responsibility
Integrate secret, dependency, SAST, dangerous-diff and supply-chain scans as evidence-generating graph nodes without granting scanners control-plane authority.

## 2. Non-negotiable design rules
- Scanner failure is distinct from finding.
- Required scanner findings gate completion/release through policy/criteria.
- Scanner output is untrusted external/tool data.

## 3. Components
- **SecretScanner** — diff/worktree secret checks
- **DependencyScanner** — vulnerability/license
- **StaticAnalyzer** — language-specific SAST
- **DiffRiskClassifier** — privilege/config/supply-chain changes
- **FindingNormalizer** — typed evidence

## 4. Canonical contracts
`SecurityFinding {rule,severity,confidence,location,evidence,scanner_version}`, `ScanResult`.

## 5. Failure and recovery
Timeout/tool crash yields scanner-unavailable outcome; release policy decides block. Baselines/suppressions are explicit, scoped and reviewed.

## 6. Security and trust
Scanner executables run sandboxed where practical; results cannot self-approve/suppress; secret material redacted.

## 7. Implementation notes
Risk-triggered security reviewer consumes normalized findings + diff/context and cannot downgrade deterministic high-severity gate without explicit policy.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
