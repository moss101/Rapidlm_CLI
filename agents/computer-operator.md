# Agent Profile — computer-operator

**Purpose:** browser/desktop/mobile interaction.  
**System prompt:** `system-prompts/computer-use.md`  
**Default tools:** `browser.act`, `computer.act`, `mobile.act`, `evidence.record`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
