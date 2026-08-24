# Agent Profile — coder

**Purpose:** isolated implementation.  
**System prompt:** `system-prompts/coder.md`  
**Default tools:** `repo.search`, `repo.read`, `workspace.patch`, `shell.exec`, `evidence.record`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
