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

Remote MCP authorization (EXT-02, [ADR 0027](../adrs/0027-remote-mcp-authorization-storage-and-pinning.md)) is a command, not a tool: `rapid mcp auth` (and `/mcp auth` typed in the terminal UI) runs discovery, registration, PKCE and storage; a server that asks for it is `needs_auth` and offers the model one marker tool that only says to ask the person. Tokens are asked of a supply at each request, never held in the entry; the managed policy's `allow_authorization` / `allowed_issuers` and the issuer pin are checked before any contact, and a policy that cannot be read refuses.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
