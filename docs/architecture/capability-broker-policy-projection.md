# Architecture — Capability Broker, Policy and Tool Projection

## 1. Responsibility
Centralize normalized privilege decisions, approval leases and model-visible capability projection.

## 2. Non-negotiable design rules
- Policy decision and sandbox isolation are separate layers.
- Lower-precedence policy can only narrow.
- Tool projection reduces attack/token surface but execution revalidates authority.

## 3. Components
- **PolicyEngine** — merges compiled/org/user/project/session rules
- **CapabilityBroker** — normalize request and issue lease
- **ApprovalQueue** — durable human decisions
- **CapabilityProjection** — role/node/mode/model tool subset
- **LeaseVerifier** — executor-side enforcement

## 4. Canonical contracts
`CapabilityRequest`, `PolicyDecision {allow,ask,deny}`, `CapabilityLease`, `ApprovalRequest`, `ToolProjection`.

## 5. Failure and recovery
Interactive Ask waits as durable Approval node; `dont-ask` returns denial. Expired/revoked lease fails and can trigger safe replan. Policy parse failure defaults restrictive.

## 6. Security and trust
Project rules cannot broaden user/org policy; hooks/plugins/MCP cannot self-approve. Sensitive target/action hash binds lease.

## 7. Implementation notes
Keep normalized capability vocabulary stable across tool/provider adapters; cache only decisions whose inputs/policy generations are unchanged.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
