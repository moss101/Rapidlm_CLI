# Engineering Definition of Done

A task is done only when: implementation matches referenced architecture/API; public schema changes have compatibility fixtures; privileged paths pass broker/lease checks; focused tests pass; applicable integration/property/security tests pass; user-visible behavior is documented; telemetry has no prohibited content; error paths are explicit; no unrelated refactor is included; generated artifacts are reproducible; and the implementing agent records evidence/results in its handoff.

A module milestone additionally requires failure-injection tests, cross-platform behavior statement, performance measurement against its SLO, threat-model review, eval coverage, and successful deterministic replay where relevant.

For V2 managed-autonomy work, “done” also means the applicable ownership/privacy invariant is demonstrated: clean TaskEnvelope rather than parent-transcript cloning; unique writable WorkspaceView per writer; execution-generation fencing across handoff; exclusive ControlLease during takeover; Knowledge/Playbooks incapable of granting authority; trajectory export data-policy/redaction checks; or Computer Use semantic-target/stale-action/sensitive-UI/evidence tests. Long-horizon components must include bounded-state/restart behavior rather than only short happy-path tests.
