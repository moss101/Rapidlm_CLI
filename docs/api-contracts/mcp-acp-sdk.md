# API Contract — MCP, ACP and SDK Contract

External interoperability without policy/kernel bypass.

## Types

### `McpCatalogRevision`
server tools/resources/prompts + trust/auth metadata

### `AcpSession`
session/run mapping and progress capabilities

### `SdkClient`
typed KernelClient facade

## Operations

- `mcp.list/call/refresh`
- `rapid acp`
- `sdk.sessions/runs/graphs/events/approvals`

## Error/recovery semantics

Disconnect/catalog mismatch is typed; stdio stdout remains framing-only. Reconnect uses cursors where possible.

## Versioning/compatibility

Protocol version negotiation isolated in adapters; kernel domain model does not encode vendor protocol versions.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
