# Security Evals

Covers prompt injection, project trust, path/symlink escape, policy precedence, approval bypass, dont-ask, stale/replayed leases, secret exfiltration/logging, MCP/plugin/hook escalation, remote result forgery, handoff split brain, credential cache cross-environment, resource pool cross-tenant data, archive traversal, sandbox downgrade and egress attempt receipts.
## Common gate
Run against production contracts. Prefer deterministic assertions before model judges. Record exact fixture/version/model/prompt/context/tool schemas. A security/data-loss/duplicate-effect/false-completion regression is a hard failure regardless of aggregate score.
