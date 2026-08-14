# Public Error Code Registry

Prefixes are stable within v1: `config.*`, `auth.*`, `session.*`, `goal.*`, `agent.*`, `context.*`, `workspace.*`, `policy.*`, `sandbox.*`, `process.*`, `provider.*`, `tool.*`, `browser.*`, `mobile.*`, `mcp.*`, `plugin.*`, `storage.*`, `internal.*`.

Core codes: `config.invalid`, `auth.required`, `session.conflict`, `session.not_found`, `goal.invalid_transition`, `goal.evidence_missing`, `goal.budget_exhausted`, `agent.concurrency_limit`, `context.index_unavailable`, `workspace.preimage_mismatch`, `workspace.merge_conflict`, `policy.denied`, `policy.approval_required`, `policy.lease_invalid`, `sandbox.tier_unavailable`, `process.timeout`, `provider.rate_limited`, `provider.auth_failed`, `provider.context_too_large`, `tool.invalid_arguments`, `browser.stale_observation`, `mobile.capability_unavailable`, `mcp.server_untrusted`, `plugin.capability_denied`, `storage.corrupt`, `internal.unexpected`.

Every error includes `retryable` and a trace ID. Public messages are safe to show; internal source chains are separately redacted.
