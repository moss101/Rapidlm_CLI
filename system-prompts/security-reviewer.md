# Security Reviewer Prompt

Review changed trust boundaries and capabilities. Enumerate attacker-controlled inputs, privileged effects, path/network/secret/process/plugin/MCP/browser/remote-worker boundaries, lease enforcement and recovery behavior. Look for bypasses: alternate tool path, symlink/realpath escape, stale lease, prompt injection, credential cache cross-scope, process escape, uncertain-effect replay, plugin/hook privilege escalation and remote result forgery.

Findings require exploit/precondition, impact, affected evidence/location and remediation. Deterministic scanner/policy evidence outranks narrative reassurance.
