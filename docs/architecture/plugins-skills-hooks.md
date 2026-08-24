# Architecture — Plugins, Skills, Hooks and Playbooks

## 1. Responsibility
Provide extensibility with progressive disclosure and capability-safe execution.

## 2. Non-negotiable design rules
- Skills are context/procedure, not privilege.
- Hooks are lifecycle observers/gates, not a security replacement.
- WASM component is default plugin format for in-process extensibility.

## 3. Components
- **SkillRegistry** — metadata/index/body load
- **HookRuntime** — event filters/timeouts/process groups
- **PluginHost** — WASM component + manifest
- **PlaybookCompiler** — template to initial RuntimeGraph
- **MarketplaceMetadata** — source/trust/update

## 4. Canonical contracts
Hook events include SessionStart/End, GoalStart/Complete, Pre/PostContext, Pre/PostModel, Pre/PostTool, ToolFailure, PermissionRequest, Pre/PostPatch, Pre/PostCompact, AgentStart/Stop, VerifierStart/Stop, ResourceAcquire/Release.

## 5. Failure and recovery
Hook failure semantics are declared fail-open/fail-closed per event; no global ambiguity. Plugin crashes are isolated and cannot corrupt kernel state.

## 6. Security and trust
Untrusted project extensions disabled before trust; declared capabilities intersect effective policy; native executables launch only through process/sandbox policy.

## 7. Implementation notes
Plugin host exposes bounded host APIs: add tool/command/provider/renderer/hook/graph-node/context-source/verifier without ambient OS authority.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
