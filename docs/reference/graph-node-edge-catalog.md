# Graph Node / Edge Catalog

## Nodes
Goal, Criterion, Plan, Task, Agent, ContextQuery, ContextPacket, ToolInvocation, Process, Monitor, ResourceAcquire, WorkspaceTransaction, ComputerObservation, ComputerAction, Preview, Approval, AskUser, Trigger, Artifact, Claim, Evidence, Verification, Handoff, HumanControl, Join, Barrier.

## Edges
DecomposesInto, DependsOn, Blocks, ScheduledAfter, JoinsAt, ProvidesContextTo, Reads, Writes, Mutates, Produces, Supports, Contradicts, Verifies, RequiresApproval, DelegatedTo, Supersedes, Invalidates, TriggeredBy.

Executable dependency edges must remain acyclic per revision. Historical/supersession/evidence relations may form richer graph structures in their specialized fabrics but traversal is bounded and typed.
