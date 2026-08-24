# RapidLM V3 Change Log — Architecture Delta from V2

1. **Runtime Graph promoted to orchestration authority.** Agent loop/coordinator becomes executor/planner, not global state owner.
2. Dynamic immutable graph revisions, host-validated proposals, repair/replan and typed invalidation.
3. Context Engine promoted to Context Fabric with InformationNeed, Context Scout, exhaustive negative-findings protocol, visibility lineage and stale-context invalidation.
4. Tool Contract Repair Engine: validate original → localized issue-path repair → revalidate; plus cross-tool invariant engine and recovery-oriented outcomes.
5. Goal completion upgraded to explicit CompletionCandidate + independent verifier + fresh evidence graph predicates.
6. Background tasks use Monitor/Trigger wake events rather than LLM polling.
7. Sandbox/remote execution extended with warm ResourcePool and phase telemetry.
8. Computer Use gains dedicated driver abstraction, coordinate normalization, safe batching, settle/reobserve and screenshot-on-failure; PreviewSupervisor becomes first-class.
9. Durable Approval/AskUser/external-condition graph waiting nodes.
10. Session graph/workspace rewind/fork/time travel without destructive Git history rewrite.
11. CredentialBroker for short-lived environment-scoped credentials.
12. Capability Projection reduces model tool surface but does not replace execution policy.
13. Preference Fabric adds provenance/confidence/scope/decay soft learning from accept/reject/edit.
14. External coding agents may run behind AgentExecutor adapters while RapidLM retains graph/policy/workspace/evidence authority.
15. Eval Harness expanded for graph/tool repair/context/open-model/resource/preview/adversarial and long-horizon metrics.
