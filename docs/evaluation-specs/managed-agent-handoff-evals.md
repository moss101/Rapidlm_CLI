# Managed Agent and Execution Handoff Evaluation Specification

## Suites

### MA-1 Clean-context delegation

Assert child TaskEnvelope contains required files/rules/criteria but excludes unrelated parent transcript tokens. Measure `context_duplication_tokens`.

### MA-2 Persistent background benefit

Run 20-turn repository task with/without persistent Explorer. Candidate must not regress correctness and should reduce repeated search/read tokens by >=15% on held-out suite before default enablement.

### MA-3 Isolated writers

Spawn three write agents targeting overlapping modules. Assert unique writable WorkspaceViews and no direct shared file writes.

### MA-4 Child crash/recovery

Kill worker after patch-before-result and result-before-parent-ingest boundaries. Recovery must preserve attribution and never apply unverified patch automatically.

### HO-1 Local→remote handoff

Transfer active paused goal, uncommitted ChangeSet and artifact refs. Verify target state equality by canonical projection hash.

### HO-2 Split-brain fault injection

Inject network/process failure at every handoff state. At no point may source and target both pass write-lease validation.

### HO-3 Capability non-transfer

Issue source lease then hand off. Target executor must reject source lease and require target policy authorization.

### HC-1 Human takeover

Human writes file and interacts with browser while agent write domains are paused. Resume must detect mutations and force fresh UI observation.
