# API Contract — Agent Executor API

Run native or external Agent nodes under a common clean-context/result contract.

## Types

### `AgentExecutionContext`
role/task/context/tools/workspace/policy/model/budgets/evidence obligations

### `AgentResult`
outcome, claims, evidence, artifacts, transaction, blockers/open questions/usage

### `AgentCapabilities`
tool/vision/structured/context/background properties

## Operations

- `agent.execute(context)`
- `agent.cancel(attempt)`
- `agent.checkpoint(attempt)`
- `agent.restore(checkpoint)`
- `agent.capabilities(executor)`

## Error/recovery semantics

Provider/runtime failures park/fail node by typed class; cancellation is cooperative then forced at supervisor boundary. External agent result never becomes authoritative change automatically.

## Versioning/compatibility

Executor implementations may vary; `AgentResult` and graph ownership remain stable.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
