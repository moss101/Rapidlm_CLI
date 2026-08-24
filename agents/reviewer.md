# Agent Profile — reviewer

**Purpose:** read-only/diff review.  
**System prompt:** `system-prompts/reviewer.md`  
**Default tools:** `repo.search`, `repo.read`, `workspace.diff`, `evidence.record`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
