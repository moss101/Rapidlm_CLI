# V2 → V3 Migration Strategy

V3 is a **migration over rewrite**.

| V2 concept | V3 action |
|---|---|
| Kernel / service lifecycle | KEEP, add GraphService authority |
| Event Ledger | KEEP/EXTEND for graph revisions/new events |
| Goal DAG | ADAPT into Goal/Criterion/Evidence portions of Runtime Graph |
| Coordinator + Managed Agent Mesh | ADAPT: planning is Agent node; scheduling moves to Graph Scheduler |
| Agent loop | KEEP as AgentNodeExecutor algorithm |
| Context Engine | KEEP/EXTEND into Context Fabric; add InformationNeed/Scout/lineage/visibility invalidation |
| Tool Gateway | KEEP/EXTEND with validate-repair-revalidate and cross-tool invariants |
| Capability Broker / leases | KEEP/EXTEND with tool projection/dont-ask/credentials |
| WorkspaceViews / semantic patch | KEEP/EXTEND with graph refs, rewind and stronger change attribution |
| Process/jobs | KEEP/EXTEND with Monitor/Trigger nodes and no-poll wake events |
| Sandbox/remote workers | KEEP/EXTEND with warm ResourcePool and phase telemetry |
| Computer Use V2 | KEEP/EXTEND with dedicated driver, normalize/batch/settle/failure diagnostics, PreviewSupervisor |
| Handoff/ControlLease | KEEP, integrate graph/resource generations |
| MCP/ACP/hooks/plugins/skills | KEEP/EXTEND; graph/policy authority remains host |
| Eval/trajectory/Session Insights | KEEP/EXTEND to graph/tool-repair/context/preference metrics |

## Mandatory Phase 0 output

Actual repository audit produces one row per current module/crate: `KEEP | ADAPT | REPLACE | DELETE`, exact owner, target owner, migrations, compatibility tests and deletion gate. The dossier never authorizes deleting a working primitive merely because the V3 name differs.
