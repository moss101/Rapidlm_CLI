# MCP, ACP and SDK Boundary Contracts

## MCP

RapidLM targets MCP `2026-07-28`. The MCP client maintains a deterministic server/tool catalog outside the model-visible tool schema and exposes calls through `external.call`. Tool discovery changes update the catalog cache, not the model tool names. Server trust, tool risk classification and credential scope are policy inputs.

RapidLM can also expose selected tools/resources as an MCP server; server mode never leaks host-only capabilities that policy did not explicitly publish.

## ACP

ACP runs over stdio by default. Implement version/capability negotiation for ACP v1 and v2. ACP is a frontend adapter over `KernelClient`; it does not own sessions or permissions independently.

## TypeScript SDK

```ts
const client = await RapidClient.connect({ transport: 'local' });
const session = await client.sessions.create({ project: process.cwd() });
for await (const event of session.run({ prompt: 'Fix the failing test' })) {
  // typed Event union
}
```

SDK semver tracks wire compatibility, not Rust crate internals. Generated schemas are checked into `sdk/typescript/src/generated` and contract-tested against Rust fixtures.
