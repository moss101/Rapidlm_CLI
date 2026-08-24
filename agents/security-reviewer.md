# Agent Profile — security-reviewer

**Purpose:** security/trust boundary audit.  
**System prompt:** `system-prompts/security-reviewer.md`  
**Default tools:** `repo.search`, `repo.read`, `shell.exec`, `evidence.record`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
