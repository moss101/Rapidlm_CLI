# Daemon RPC

`rapid daemon` serves the SDK's wire contract on a Unix socket. It is **not** an HTTP server, so there is no OpenAPI
description; the contract is described in OpenRPC, the JSON-RPC counterpart, at
[`docs/api-contracts/sdk-rpc.openrpc.json`](../api-contracts/sdk-rpc.openrpc.json).

It documents the handshake (`hello` / `hello_ok`, the daemon token, the wire-catalog hash), every method
(`sessions.create|get|fork|rewind`, `dashboard.get`, `turns.submit|interrupt`, `approvals.resolve`, `events.subscribe`),
the streamed `event` and `stream_end` frames, the `cancel` frame, and the exact session snapshot the SDK decodes.

The document cannot drift from the daemon unnoticed: `cargo test -p rapid --lib contract_tests` fails if a method is
served and not documented (or the reverse), if a status or goal enum differs from `sdk/typescript/schemas/wire.v1.json`,
if a `$ref` dangles, or if the session fields differ from what the SDK's `decodeSession` accepts.

A conversation, one JSON object per line:

```
-> {"schema":"rapidlm.sdk.rpc","schema_version":1,"kind":"hello","id":1,"auth":{"handle":"<daemon.token>"}}
<- {"schema":"rapidlm.sdk.rpc","schema_version":1,"kind":"hello_ok","id":1,"wire_schema":"rapidlm.sdk.wire","wire_schema_version":1,"wire_schema_sha256":"..."}
-> {"schema":"rapidlm.sdk.rpc","schema_version":1,"kind":"request","id":2,"method":"sessions.create","params":{}}
<- {"schema":"rapidlm.sdk.rpc","schema_version":1,"kind":"response","id":2,"result":{"schema":1,"id":"...","status":"ready","seq":2,...}}
```
