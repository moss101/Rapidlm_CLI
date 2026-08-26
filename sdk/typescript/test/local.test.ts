import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { inspect } from "node:util";

import {
  DecodeError,
  MAX_EVENT_BYTES,
  WIRE_CATALOG_SCHEMA,
  WIRE_SCHEMA_SHA256,
  WIRE_SCHEMA_VERSION,
  decodeEvent,
} from "../src/generated/index.ts";
import {
  LocalTransport,
  MAX_RPC_FRAME_BYTES,
  RPC_SCHEMA,
  RPC_SCHEMA_VERSION,
  RpcError,
  TransportError,
  createMemoryChannelPair,
} from "../src/transport/local.ts";
import type { TransportChannel } from "../src/transport/local.ts";

const SDK_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const LOCAL_SRC = join(SDK_ROOT, "src", "transport", "local.ts");

const GOLDEN_UUID = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
const SESSION_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
const SECRET_CANARY = "hunter2-capability-lease";
const AUTH_TOKEN = "local-auth-handle-not-for-logs";

const PROTOCOL_ERROR = {
  code: "policy.denied",
  message: "Action denied by project policy",
  retryable: false,
  trace_id: GOLDEN_UUID,
  details: {},
};

function goldenEvent(seq: number) {
  return {
    schema: 1,
    event_id: GOLDEN_UUID,
    session_id: SESSION_ID,
    seq,
    recorded_at: "2026-08-14T15:20:04.123Z",
    actor: { kind: "agent", id: GOLDEN_UUID },
    trace_id: GOLDEN_UUID,
    kind: "tool.completed",
    redaction: "project",
    payload: { call_id: "call_7" },
  };
}

type Peer = {
  send(obj: unknown): void;
  sendRaw(line: string): void;
  disconnect(): void;
  recv(): Promise<Record<string, unknown>>;
};

function attachPeer(server: TransportChannel): Peer {
  const buffered: Record<string, unknown>[] = [];
  const waiters: Array<(value: Record<string, unknown>) => void> = [];
  let closed = false;
  void (async () => {
    try {
      for await (const line of server.frames()) {
        const parsed = JSON.parse(line) as Record<string, unknown>;
        const waiter = waiters.shift();
        if (waiter) {
          waiter(parsed);
        } else {
          buffered.push(parsed);
        }
      }
    } finally {
      closed = true;
    }
  })();
  return {
    send(obj: unknown): void {
      server.send(JSON.stringify(obj));
    },
    sendRaw(line: string): void {
      server.send(line);
    },
    disconnect(): void {
      server.close();
    },
    async recv(): Promise<Record<string, unknown>> {
      const next = buffered.shift();
      if (next) {
        return next;
      }
      if (closed) {
        throw new Error("mock daemon closed");
      }
      return new Promise((resolve) => {
        waiters.push(resolve);
      });
    },
  };
}

function createBroker(): {
  factory: () => TransportChannel;
  nextPeer: () => Promise<Peer>;
  connectCount: () => number;
} {
  const waiting: Array<(peer: Peer) => void> = [];
  const ready: Peer[] = [];
  let connects = 0;
  return {
    factory(): TransportChannel {
      connects += 1;
      const { client, server } = createMemoryChannelPair();
      const peer = attachPeer(server);
      const waiter = waiting.shift();
      if (waiter) {
        waiter(peer);
      } else {
        ready.push(peer);
      }
      return client;
    },
    nextPeer(): Promise<Peer> {
      const peer = ready.shift();
      if (peer) {
        return Promise.resolve(peer);
      }
      return new Promise((resolve) => {
        waiting.push(resolve);
      });
    },
    connectCount(): number {
      return connects;
    },
  };
}

function helloOk(id: unknown, overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    schema: RPC_SCHEMA,
    schema_version: RPC_SCHEMA_VERSION,
    kind: "hello_ok",
    id,
    wire_schema: WIRE_CATALOG_SCHEMA,
    wire_schema_version: WIRE_SCHEMA_VERSION,
    wire_schema_sha256: WIRE_SCHEMA_SHA256,
    ...overrides,
  };
}

async function acceptHello(peer: Peer): Promise<Record<string, unknown>> {
  const hello = await peer.recv();
  assert.equal(hello.kind, "hello");
  peer.send(helloOk(hello.id));
  return hello;
}

async function connectMock(): Promise<{
  transport: LocalTransport;
  peer: Peer;
  broker: ReturnType<typeof createBroker>;
}> {
  const broker = createBroker();
  const connecting = LocalTransport.connect({
    channelFactory: broker.factory,
    authToken: AUTH_TOKEN,
    reconnect: { maxAttempts: 3, backoffMs: 0 },
  });
  const peer = await broker.nextPeer();
  await acceptHello(peer);
  return { transport: await connecting, peer, broker };
}

function assertNoCanary(value: unknown, canary: string): void {
  const rendered = `${inspect(value, { depth: 8 })}\n${String(value)}\n${JSON.stringify(value)}`;
  assert.equal(rendered.includes(canary), false, `leaked canary: ${rendered}`);
}

describe("LocalTransport", () => {
  test("requires local auth before opening a channel", async () => {
    let called = false;
    await assert.rejects(
      () =>
        LocalTransport.connect({
          channelFactory: () => {
            called = true;
            return createMemoryChannelPair().client;
          },
        }),
      (err: unknown) => err instanceof TransportError && err.code === "auth.required",
    );
    assert.equal(called, false);
  });

  test("rejects remote websocket URLs before connect (T-005)", async () => {
    await assert.rejects(
      () =>
        LocalTransport.connect({
          websocketUrl: "ws://169.254.169.254/sdk",
          authToken: AUTH_TOKEN,
        }),
      (err: unknown) => err instanceof TransportError && err.code === "protocol",
    );
    await assert.rejects(
      () =>
        LocalTransport.connect({
          websocketUrl: "ws://localhost/sdk",
          authToken: AUTH_TOKEN,
        }),
      (err: unknown) => err instanceof TransportError && err.code === "protocol",
    );
    await assert.rejects(
      () =>
        LocalTransport.connect({
          websocketUrl: "ws://127.0.0.1.attacker.example/sdk",
          authToken: AUTH_TOKEN,
        }),
      (err: unknown) => err instanceof TransportError && err.code === "protocol",
    );
    await assert.rejects(
      () =>
        LocalTransport.connect({
          websocketUrl: `ws://user:${SECRET_CANARY}@127.0.0.1/sdk`,
          authToken: AUTH_TOKEN,
        }),
      (err: unknown) => {
        assert.ok(err instanceof TransportError);
        assert.equal(err.code, "protocol");
        assertNoCanary(err, SECRET_CANARY);
        return true;
      },
    );
  });

  test("rejects URL-shaped IPC paths and does not parse sqlite/repo files", async () => {
    const src = readFileSync(LOCAL_SRC, "utf8");
    assert.equal(src.includes("node:fs"), false);
    assert.equal(src.includes("node:sqlite"), false);
    assert.equal(src.includes("better-sqlite"), false);
    assert.equal(/from ["'].*sqlite/i.test(src), false);
    await assert.rejects(
      () =>
        LocalTransport.connect({
          socketPath: "ws://127.0.0.1/sdk",
          authToken: AUTH_TOKEN,
        }),
      (err: unknown) => err instanceof TransportError && err.code === "protocol",
    );
  });

  test("handshakes with unique request ids and hides the auth handle", async () => {
    const { transport, peer } = await connectMock();
    assert.equal(transport.connected, true);
    assert.equal(inspect(transport).includes(AUTH_TOKEN), false);
    assert.equal(JSON.stringify(transport), "{}");

    const req = transport.request("sessions.get", { session_id: SESSION_ID });
    const frame = await peer.recv();
    assert.equal(frame.kind, "request");
    assert.equal(frame.method, "sessions.get");
    assert.match(String(frame.id), /^[0-9a-f-]{36}$/);
    assert.equal(JSON.stringify(frame).includes(AUTH_TOKEN), false);

    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "response",
      id: frame.id,
      result: { id: SESSION_ID },
    });
    const result = await req;
    assert.deepEqual(result, { id: SESSION_ID });
    await transport.close();
  });

  test("abort sends cancel for the same request id", async () => {
    const { transport, peer } = await connectMock();
    const controller = new AbortController();
    const pending = transport.request(
      "sessions.get",
      { session_id: SESSION_ID },
      { signal: controller.signal },
    );
    const request = await peer.recv();
    assert.equal(request.kind, "request");
    controller.abort();
    const cancel = await peer.recv();
    assert.equal(cancel.kind, "cancel");
    assert.equal(cancel.id, request.id);
    await assert.rejects(
      pending,
      (err: unknown) => err instanceof TransportError && err.code === "cancelled",
    );

    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "response",
      id: request.id,
      result: { ignored: true },
    });
    await transport.close();
  });

  test("decodes typed ApiError and fails closed on malformed errors", async () => {
    const { transport, peer } = await connectMock();
    const denied = transport.request("turns.submit", { session_id: SESSION_ID, expected_seq: 1 });
    const first = await peer.recv();
    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "response",
      id: first.id,
      error: PROTOCOL_ERROR,
    });
    await assert.rejects(denied, (err: unknown) => {
      assert.ok(err instanceof RpcError);
      assert.equal(err.error.code, "policy.denied");
      assert.equal(err.error.retryable, false);
      return true;
    });

    const malformed = transport.request("sessions.get", { session_id: SESSION_ID });
    const second = await peer.recv();
    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "response",
      id: second.id,
      error: { ...PROTOCOL_ERROR, retryable: true },
    });
    await assert.rejects(malformed, (err: unknown) => {
      assert.ok(err instanceof TransportError);
      assert.equal(err.code, "protocol");
      assert.ok(err.cause instanceof DecodeError);
      return true;
    });

    const secret = transport.request("sessions.get", { session_id: SESSION_ID });
    const third = await peer.recv();
    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "response",
      id: third.id,
      error: {
        ...PROTOCOL_ERROR,
        details: { cause: SECRET_CANARY },
      },
    });
    await assert.rejects(secret, (err: unknown) => {
      assert.ok(err instanceof TransportError);
      assertNoCanary(err, SECRET_CANARY);
      return true;
    });
    await transport.close();
  });

  test("subscribe reconnects from event cursor and skips duplicates", async () => {
    const { transport, peer, broker } = await connectMock();
    const events: number[] = [];
    const consumed = new Promise<void>((resolve, reject) => {
      void (async () => {
        try {
          for await (const event of transport.subscribe({
            session_id: SESSION_ID,
            from_seq: 0,
          })) {
            events.push(event.seq);
            if (event.seq === 3) {
              resolve();
              return;
            }
          }
          reject(new Error("subscription ended early"));
        } catch (err) {
          reject(err);
        }
      })();
    });

    const sub = await peer.recv();
    assert.equal(sub.kind, "request");
    assert.equal(sub.method, "events.subscribe");
    assert.deepEqual(sub.params, { session_id: SESSION_ID, from_seq: 0 });

    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "event",
      id: sub.id,
      cursor: 1,
      event: goldenEvent(1),
    });
    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "event",
      id: sub.id,
      cursor: 2,
      event: goldenEvent(2),
    });

    await waitUntil(() => events.length === 2);
    assert.deepEqual(events, [1, 2]);

    peer.disconnect();
    const peer2 = await broker.nextPeer();
    const hello2 = await acceptHello(peer2);
    assert.equal(hello2.kind, "hello");
    assert.notEqual(hello2.id, sub.id);

    const sub2 = await peer2.recv();
    assert.equal(sub2.method, "events.subscribe");
    assert.deepEqual(sub2.params, { session_id: SESSION_ID, from_seq: 2 });
    assert.notEqual(sub2.id, sub.id);

    peer2.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "event",
      id: sub2.id,
      cursor: 2,
      event: goldenEvent(2),
    });
    peer2.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "event",
      id: sub2.id,
      cursor: 3,
      event: goldenEvent(3),
    });

    await consumed;
    assert.deepEqual(events, [1, 2, 3]);
    assert.equal(decodeEvent(goldenEvent(3)).seq, 3);
    assert.equal(broker.connectCount(), 2);
    await transport.close();
  });

  test("does not replay an in-flight unary request after disconnect", async () => {
    const { transport, peer, broker } = await connectMock();
    const pending = transport.request("turns.submit", {
      session_id: SESSION_ID,
      expected_seq: 1,
    });
    const request = await peer.recv();
    const requestId = request.id;
    peer.disconnect();
    await assert.rejects(
      pending,
      (err: unknown) => err instanceof TransportError && err.code === "unknown_outcome",
    );
    assert.equal(broker.connectCount(), 1);

    const retry = transport.request("turns.submit", {
      session_id: SESSION_ID,
      expected_seq: 1,
    });
    const peer2 = await broker.nextPeer();
    await acceptHello(peer2);
    const second = await peer2.recv();
    assert.equal(second.method, "turns.submit");
    assert.notEqual(second.id, requestId);
    peer2.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "response",
      id: second.id,
      result: { seq: 2 },
    });
    assert.deepEqual(await retry, { seq: 2 });
    await transport.close();
  });

  test("fails closed on schema mismatch, oversized frames, and unknown methods", async () => {
    const broker = createBroker();
    const connecting = LocalTransport.connect({
      channelFactory: broker.factory,
      authToken: AUTH_TOKEN,
    });
    const peer = await broker.nextPeer();
    const hello = await peer.recv();
    peer.send(
      helloOk(hello.id, {
        wire_schema_version: 2,
      }),
    );
    await assert.rejects(
      connecting,
      (err: unknown) => err instanceof TransportError && err.code === "unsupported_schema",
    );
    assertNoCanary(await connecting.then(() => null).catch((err) => err), AUTH_TOKEN);

    const { transport, peer: peer2 } = await connectMock();
    await assert.rejects(
      () => transport.request("events.subscribe", { session_id: SESSION_ID, from_seq: 0 }),
      (err: unknown) => err instanceof TransportError && err.code === "protocol",
    );

    const pending = transport.request("sessions.get", { session_id: SESSION_ID });
    await peer2.recv();
    peer2.sendRaw("x".repeat(MAX_RPC_FRAME_BYTES + 1));
    await assert.rejects(
      pending,
      (err: unknown) =>
        err instanceof TransportError &&
        (err.code === "oversized" || err.code === "unknown_outcome"),
    );
    await transport.close();

    assert.ok(MAX_EVENT_BYTES >= MAX_RPC_FRAME_BYTES);
  });

  test("auth failure is typed and does not leak the handle", async () => {
    const broker = createBroker();
    const connecting = LocalTransport.connect({
      channelFactory: broker.factory,
      authToken: AUTH_TOKEN,
    });
    const peer = await broker.nextPeer();
    const hello = await peer.recv();
    assert.equal((hello.auth as { handle?: string }).handle, AUTH_TOKEN);
    peer.send({
      schema: RPC_SCHEMA,
      schema_version: RPC_SCHEMA_VERSION,
      kind: "response",
      id: hello.id,
      error: {
        code: "auth.required",
        message: "Authentication required",
        retryable: false,
        trace_id: GOLDEN_UUID,
        details: {},
      },
    });
    await assert.rejects(connecting, (err: unknown) => {
      assert.ok(err instanceof RpcError);
      assert.equal(err.error.code, "auth.required");
      assertNoCanary(err, AUTH_TOKEN);
      return true;
    });
  });
});

async function waitUntil(predicate: () => boolean, timeoutMs = 1000): Promise<void> {
  const start = Date.now();
  while (!predicate()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error("timed out waiting for condition");
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}
