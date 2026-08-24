# Incident and Recovery Runbook

For corruption/crash: preserve diagnostic bundle and working tree, stop new mutable scheduling, inspect Event Ledger/checkpoint/Operation Journal, reconcile uncertain effects, restore projections from last valid checkpoint/replay, validate workspace/resource/lease generations, park unresolved goals blocked/paused, resume only after invariant checks. Never delete user work or rewrite Git history as automatic recovery.
