# Stable Tool Gateway Contract

The model sees a deliberately small and stable catalog. Provider adapters map native function-calling formats to these canonical requests.

## Tool request

```json
{
  "schema": 1,
  "call_id": "call_7",
  "tool": "repo.search",
  "arguments": {"query":"CapabilityLease","repos":["main"],"limit":20}
}
```

## Stable tools

| Tool | Purpose | Privilege boundary |
|---|---|---|
| `repo.search` | hybrid/lexical/structural search | read scope |
| `repo.read` | bounded file/range/symbol read | read scope |
| `workspace.patch` | stage semantic patch | write scope |
| `workspace.status` | diff/view/checkpoint state | read scope |
| `shell.exec` | supervised command execution | command + fs + network capability |
| `agent.spawn` | create isolated subagent | scheduler/policy |
| `agent.result` | inspect/merge typed subagent result | view/merge policy |
| `goal.update` | machine goal lifecycle/evidence linkage | main-agent-only |
| `browser.act` | observe/act/verify browser | browser origin/action policy |
| `mobile.act` | simulator actions | device/simulator policy |
| `external.call` | MCP/plugin capability proxy | external server/tool policy |
| `evidence.record` | attach verifiable evidence | evidence validation |

## Result envelope

```json
{
  "schema":1,
  "call_id":"call_7",
  "status":"ok",
  "summary":"12 matches",
  "data":{},
  "artifacts":[],
  "truncated":false,
  "continuation":null
}
```

Large output is placed in an artifact and represented with a bounded excerpt. Tool implementations MUST NOT return hidden system prompts, raw credentials, or capability tokens to the model.
