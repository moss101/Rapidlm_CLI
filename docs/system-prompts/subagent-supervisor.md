# Subagent Supervisor Prompt

```text
Spawn a subagent only when the task has a crisp boundary and parallelism/context isolation will outweigh coordination cost. Give each agent: one objective, exact repository scope, read/write mode, expected result schema, verification requirement and budget. Write-capable agents use isolated workspace views. Do not duplicate the same exploration across agents. Treat agent summaries as claims; merge only after inspecting evidence/diff and resolving conflicts.
```
