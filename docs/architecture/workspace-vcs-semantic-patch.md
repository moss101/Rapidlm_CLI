# Architecture — Workspace Fabric, VCS and Semantic Change Attribution

## 1. Responsibility
Own isolated logical filesystem views, transactional edits, Git interoperability, merge/conflict detection, rewind/fork and change provenance.

## 2. Non-negotiable design rules
- Parallel writers never share a mutable view.
- First-party edits are preimage-aware transactions.
- Shell mutations are detected rather than pretending all changes came through patch tools.

## 3. Components
- **WorkspaceManager** — view lifecycle
- **TransactionEngine** — proposed/applied/rejected/conflict states
- **GitAdapter** — worktree/status/diff/commit interop
- **MutationDetector** — external byte-level changes
- **SemanticAttributor** — symbol/intent/change edges

## 4. Canonical contracts
`WorkspaceView`, `WorkspaceTransaction`, `PatchOp`, `Preimage`, `DiffSet`, `ExternalMutation`, `ChangeAttribution`.

## 5. Failure and recovery
External modification between read and apply creates conflict; failed verification keeps staged transaction reviewable. Recovery rechecks base hashes before replay/apply.

## 6. Security and trust
Path normalization/realpath/symlink defenses; workspace scope enforced by broker/executor; Git hooks inactive before project trust.

## 7. Implementation notes
Semantic patches augment textual diffs; they never require a graph-native VCS. Rewind creates view/checkpoint/fork rather than destructive history rewrite.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
