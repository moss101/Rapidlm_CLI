# Agent Profile — context-scout

**Purpose:** read-only exhaustive code investigation.  
**System prompt:** `system-prompts/context-scout.md`  
**Default tools:** `repo.search`, `repo.read`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
