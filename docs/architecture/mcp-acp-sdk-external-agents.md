# Architecture — MCP, ACP, SDK and External Agent Adapters

## 1. Responsibility
Integrate tool ecosystems/editors/embedding applications and optional external coding-agent executors without surrendering RapidLM graph/policy authority.

## 2. Non-negotiable design rules
- ACP/MCP are adapters, not alternate kernels.
- External tool/agent output is untrusted.
- stdout is protocol-owned for stdio modes.

## 3. Components
- **McpClient** — catalog/discovery/call
- **McpServer** — explicit published Rapid resources
- **AcpServer** — stdio session/progress/diff/permission
- **TypeScriptSDK** — stable API/events
- **ExternalAgentAdapter** — ACP/CLI execution behind Agent node contract

## 4. Canonical contracts
`ExternalCall`, `CatalogRevision`, `AcpSession`, `SdkEvent`, `ExternalAgentCapabilities`, `ExternalAgentTask`.

## 5. Failure and recovery
Malformed/disconnected protocol parks/fails node without crashing kernel. Catalog revision mismatch requires refresh. External-agent process loss returns bounded result/recovery.

## 6. Security and trust
Every external invocation passes Capability Broker; output trust-labeled/sanitized. No model can activate an untrusted plugin/server merely through content.

## 7. Implementation notes
MCP middleware chain: schema normalize → capability map → pre-hook → policy/secret resolve → execute → output sanitize → evidence/event.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
