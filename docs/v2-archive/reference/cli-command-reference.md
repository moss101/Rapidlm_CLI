# Planned CLI / TUI Command Surface

```text
rapid                         # interactive TUI
rapid run [PROMPT]            # headless single turn/goal
rapid run --jsonl             # machine protocol
rapid resume [SESSION]
rapid sessions list|show|fork|delete
rapid goal show|start|pause|resume|cancel|budget
rapid agents list|show|pause|resume|sleep|cancel|terminate
rapid jobs list|show|cancel|logs
rapid context status|search|reindex|inspect
rapid knowledge list|show|suggest|approve|reject|edit
rapid playbook list|show|run|validate
rapid automations list|show|run|pause|resume
rapid diff [--agent ID]
rapid apply [--agent ID]
rapid rollback [CHECKPOINT]
rapid model list|select|doctor
rapid mcp list|add|remove|auth|doctor
rapid plugins list|install|remove|permissions
rapid skills list|show|enable|disable
rapid policy explain|check
rapid sandbox doctor
rapid trace show|export
rapid insights show|analyze|proposals
rapid handoff local|daemon|remote [TARGET]
rapid attach [SESSION]
rapid takeover terminal|browser|desktop|mobile
rapid control return
rapid computer status|observe|record|test
rapid eval run|compare
rapid config show|validate|edit
rapid auth login|status|logout
rapid daemon start|status|stop
rapid acp
rapid mcp-server
rapid doctor [--repair]
rapid update [--check]
```

TUI slash commands mirror high-value lifecycle actions (`/model`, `/agents`, `/goal`, `/diff`, `/context`, `/knowledge`, `/playbook`, `/trace`, `/insights`, `/handoff`, `/takeover`, `/control-return`, `/computer`, `/jobs`, `/mcp`, `/permissions`, `/compact`, `/fork`, `/rewind`, `/quit`) but call the same kernel APIs.


## V2 command semantics

- `rapid handoff ...` is a fenced execution migration, not a file copy. The source enters `handoff_pending`, quiesces side effects, transfers a signed versioned bundle, and only the committed next `SessionExecutionLease` generation may resume writes.
- `rapid takeover ...` acquires an exclusive `ControlLease` for the selected surface. Agent input remains paused until `rapid control return` completes a fresh observation/reconciliation.
- `rapid knowledge suggest` creates a governed candidate; it does not immediately modify trusted Knowledge.
- `rapid playbook run` never grants permissions. Every side effect still passes the Capability Broker.
- `rapid computer test` executes a focused UI test plan and can emit structured assertions plus redacted screenshots/video as evidence artifacts.
