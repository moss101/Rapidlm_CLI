# Incident, Crash and Recovery Runbook

Startup recovery is automatic and conservative: open DB/migrations, verify artifact store, replay projections, park impossible active turns/goals, reconcile process/job tombstones, inspect workspace transactions, mark remote leases expired, then expose the session.

Never auto-resume an active autonomous goal after process restart. Never assume a child PID still belongs to RapidLM without process identity/start-time validation. Half-staged workspace transactions are rolled back or surfaced as `recovery_required`; they are never silently committed.

`rapid doctor --repair` may rebuild caches/indexes, reconcile orphan artifacts and repair non-canonical projections. It MUST NOT discard the event ledger or overwrite repository work without a separate destructive confirmation.

For a suspected credential leak: stop network-capable jobs, revoke relevant provider/remote credentials, preserve redacted audit records, rotate canaries/secrets, identify artifact/event exposure, then resume only after policy remediation.
