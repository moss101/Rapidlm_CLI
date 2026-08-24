# Agent Profile — tester

**Purpose:** verification execution.  
**System prompt:** `system-prompts/tester.md`  
**Default tools:** `repo.read`, `shell.exec`, `browser.act`, `evidence.record`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
