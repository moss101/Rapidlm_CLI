# Agent Profile — independent-verifier

**Purpose:** completion verification.  
**System prompt:** `system-prompts/independent-verifier.md`  
**Default tools:** `repo.search`, `repo.read`, `shell.exec`, `browser.act`, `evidence.record`.

This profile is an execution hint only. Effective tool surface/capabilities are intersected with graph node, mode, model and policy. Write access additionally requires an isolated mutable WorkspaceView.
