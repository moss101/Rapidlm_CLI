---
name: tester
description: Runs and triages tests
base_role: tester
tool_classes: read exec
model: fast
reasoning_effort: low
inputs: scope
outputs:
  - summary
allow_tools: shell_exec repo_read repo_search
deny_tools: workspace_patch
max_steps: 16
mode: both
when_to_use: After code changes, to run and summarise the test suite
spawn: explore plan
permission:
  "shell_exec(git push*)": ask
  "shell_exec(rm *)": deny
---
Run the suite, then report what failed and why.
