# Testing Strategy

Testing follows the contract pyramid: pure unit/property tests → crate contract tests → cross-crate integration tests → deterministic recorded replay → live-provider/browser/mobile evals → release smoke tests.

Rules: fake time and deterministic IDs where feasible; avoid sleeps in tests; every public wire schema has golden compatibility fixtures; every privilege boundary has negative tests; crash tests use kill/restart rather than graceful shutdown; filesystem tests include symlinks and non-UTF8 names where supported; Windows/macOS/Linux behavior differences are explicit.

Required CI lanes: `fmt/lint`, unit, schema compatibility, policy/security, Linux integration, macOS integration, Windows integration, SDK typecheck/test, deterministic eval smoke, dependency/SBOM, package/install smoke. Expensive live-model and simulator suites can be scheduled but are release-blocking before stable releases.


## V2 testing additions

### Managed agents

Every scheduler/pool change requires deterministic scripted-coordinator scenarios proving clean child context, workspace isolation, lineage, bounded mailboxes, per-agent budget accounting, cancellation and restart behavior. Parallel writer tests MUST assert that no two active writers own the same writable view.

### Execution and control handoff

Inject failures before/after source quiesce, bundle signing, transfer, target verification, execution-generation commit and source revocation. The invariant is `max(simultaneously_valid_side_effect_owners) == 1`. Human takeover tests inject concurrent agent inputs and require rejection until control is returned and the surface is freshly re-observed.

### Computer Use

Test each locator tier independently: DOM/test-id, accessibility/native controls, TUI semantics, vision and coordinates. Coordinate tests MUST mutate geometry/generation and prove stale actions fail. Security fixtures cover prompt injection in UI text, secret entry masking, uploads/downloads, auth/MFA/CAPTCHA handoff, destructive dialogs and external publication. E2E fixtures record assertions, logs, screenshots and redacted annotated video.

### Trajectory/learning

Trajectory tests prove deterministic boundary-event serialization, data-policy gating, redaction and absence of hidden-chain-of-thought requirements. Generated training environments never appear in held-out release suites. Candidate promotion requires matched baseline comparison after correctness/security hard gates.

### Long-horizon endurance

Release candidates run 1h, 4h, 12h and 24h tiers with injected compaction, provider faults, daemon restart, worker handoff and child-agent failures. At least one scenario MUST exceed 1,000 tool calls. Measure memory/context growth, duplicate reads, stuck-agent rate, ownership violations, token/cost drift and evidence completeness.
