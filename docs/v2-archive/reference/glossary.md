# Glossary

**Capability:** a typed authority such as filesystem write, process execution, network origin access, browser credential entry or MCP tool use.  
**Capability lease:** short-lived, action-bound proof that the broker authorized one concrete side effect.  
**Context packet:** token-budgeted set of prompt blocks selected for one model step.  
**Evidence:** runtime-recorded observation used to prove a completion criterion.  
**Goal:** structured autonomous objective owned by the main session.  
**Kernel:** trusted lifecycle/control plane shared by all frontends.  
**Projection:** rebuildable view derived from the event ledger.  
**Semantic patch:** typed file operation with preimage/content constraints.  
**Workspace view:** isolated logical repository version assigned to an agent.  
**Tool gateway:** stable model-visible function layer translating calls into brokered runtime capabilities.  
**Untrusted data:** content that may contain adversarial instructions but has no instruction priority.  
**Attestation:** signed/hashed provenance statement linking changes and verification evidence.

**Agent Pool:** session-scoped host for persistent background agents whose lifecycle differs from bounded task workers.  
**Managed worker:** clean-context child agent created for one bounded task through a `TaskEnvelope`; write-capable workers receive isolated workspace views.  
**TaskEnvelope:** minimal typed delegation package containing objective, criteria, selected context/Knowledge references, workspace/capability ceilings, model policy, budget and expected result schema.  
**AgentResultV2:** typed managed-worker output carrying result summary, evidence, artifacts/ChangeSets, blockers, usage and trajectory summary reference.  
**SessionExecutionLease:** generation-fenced ownership record authorizing exactly one execution host generation to perform session side effects.  
**HandoffBundle:** signed, versioned migration package containing restorable session/goal/context/workspace/agent state but no live capability leases or secret plaintext.  
**ControlLease:** exclusive authorization for either agent or human to provide input to a terminal/browser/desktop/mobile surface during a bounded generation/time interval.  
**Knowledge:** governed, scoped engineering fact/rule retrieved by explicit triggers and provenance; distinct from conversational memory and incapable of granting authority.  
**Playbook:** versioned reusable multi-step workflow whose individual side effects remain independently authorized.  
**Automation:** trigger + Playbook + durable cursor/idempotency state for non-interactive recurring/event-driven work.  
**TrainingTrajectory:** governed record of observable environment, prompt/version references, context selections, model/tool boundary events, patches, verification and metrics; excludes hidden chain-of-thought.  
**Session Insights:** evidence-linked analysis of completed/ongoing session behavior, cost, duplication, failures and improvement candidates.  
**Computer Surface:** normalized target of UI automation such as browser page, desktop, window, TUI, Android/iOS simulator or remote desktop.  
**ObservationId:** immutable identity of a Computer Use observation/generation used to reject stale visual/coordinate actions.  
**Semantic target:** DOM/test-id/accessibility/native-control/TUI target preferred over raw coordinates.  
**Visual delta:** bounded screenshot-derived change region used instead of repeatedly sending full screenshots when possible.  
