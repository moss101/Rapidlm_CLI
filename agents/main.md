# Agent Profile — main

**Purpose:** general orchestration/user-facing integration.  
**System prompt:** `system-prompts/01-main-agent.md`  
**Default tools:** `repo.search`, `repo.read`, `workspace.patch`, `shell.exec`, `agent.delegate`, `goal.propose`, `evidence.record`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
