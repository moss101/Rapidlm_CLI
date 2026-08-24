# Canonical Domain Model

```mermaid
classDiagram
  Session "1" --> "*" Run
  Run "1" --> "1" RuntimeGraph
  RuntimeGraph "1" --> "*" Node
  Goal "1" --> "*" Criterion
  Criterion --> Claim
  Claim --> Evidence
  Evidence --> VerificationRecord
  Node --> ContextPacket
  Node --> WorkspaceView
  Node --> Artifact
  WorkspaceView --> WorkspaceTransaction
  Node --> CapabilityLease
  Node --> ResourceLease
```

Operational IDs are UUIDv7 newtypes; immutable artifacts use SHA-256 digests. Canonical enums/types are defined in module contracts and must not be duplicated by frontends. Models see aliases/handles where raw IDs add no semantics.

Key entities: Session, Run, RuntimeGraph/Revision/Node/Attempt/Edge, Goal/Criterion, Claim/Evidence/Verification, ContextPacket/ReadObservation, Workspace/View/Transaction, Process/Monitor/Trigger/JobLease, Capability/Policy/Lease/Approval, Observation/Action/ControlLease, Artifact/Provenance, AgentExecution, Knowledge/Decision/Preference, Handoff/WorkLease.
