import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { inspect } from "node:util";

import {
  ClientError,
  RapidClient,
  isEventKind,
} from "../src/client.ts";
import type { Event } from "../src/generated/index.ts";
import {
  MAX_STREAM_BUFFER,
  RPC_SCHEMA,
  RPC_SCHEMA_VERSION,
  RpcError,
  TransportError,
  createMemoryChannelPair,
} from "../src/transport/local.ts";
import type { TransportChannel } from "../src/transport/local.ts";
import {
  WIRE_CATALOG_SCHEMA,
  WIRE_SCHEMA_SHA256,
  WIRE_SCHEMA_VERSION,
} from "../src/generated/index.ts";

const SDK_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const CLIENT_SRC = join(SDK_ROOT, "src", "client.ts");

const GOLDEN_UUID = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
const SESSION_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
const PROJECT_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
const TURN_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
const CHILD_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789af";
const AUTH_TOKEN = "local-auth-handle-not-for-logs";
const SECRET_CANARY = "hunter2-capability-lease";
const PROJECT = "/tmp/rapidlm-sdk-client-project";

const GOLDEN_SESSION = {
  schema: 1,
  id: SESSION_ID,
  project_id: PROJECT_ID,
  status: "ready",
  active_turn: null,
  top_level_goal: null,
  active_agents: [],
  seq: 1,
  created_at: "2026-08-14T15:20:04Z",
  updated_at: "2026-08-14T15:20:04Z",
};

const PROTOCOL_ERROR = {
  code: "policy.denied",
  message: "Action denied by project policy",
  retryable: false,
  trace_id: GOLDEN_UUID,
  details: {},
};

function goldenEvent(seq: number, kind = "tool.completed", payload: Record<string, unknown> = { call_id: "call_7" }) {
  return {
    schema: 1,
    event_id: GOLDEN_UUID,
    session_id: SESSION_ID,
    seq,
    recorded_at: "2026-08-14T15:20:04.123Z",
    actor: { kind: "agent", id: GOLDEN_UUID },
    trace_id: GOLDEN_UUID,
    kind,
    redaction: "project",
    payload,
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
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          const idx = waiters.indexOf(onFrame);
          if (idx >= 0) {
            waiters.splice(idx, 1);
          }
          reject(new Error("timed out waiting for daemon frame"));
        }, 2000);
        const onFrame = (value: Record<string, unknown>): void => {
          clearTimeout(timer);
          resolve(value);
        };
        waiters.push(onFrame);
      });
    },
  };
}

function createBroker(): {
  factory: () => TransportChannel;
  nextPeer: () => Promise<Peer>;
} {
  const waiting: Array<(peer: Peer) => void> = [];
  const ready: Peer[] = [];
  return {
    factory(): TransportChannel {
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
  };
}

function helloOk(id: unknown): Record<string, unknown> {
  return {
    schema: RPC_SCHEMA,
    schema_version: RPC_SCHEMA_VERSION,
    kind: "hello_ok",
    id,
    wire_schema: WIRE_CATALOG_SCHEMA,
    wire_schema_version: WIRE_SCHEMA_VERSION,
    wire_schema_sha256: WIRE_SCHEMA_SHA256,
  };
}

async function acceptHello(peer: Peer): Promise<Record<string, unknown>> {
  const hello = await peer.recv();
  assert.equal(hello.kind, "hello");
  peer.send(helloOk(hello.id));
  return hello;
}

async function connectClient(): Promise<{
  client: RapidClient;
  peer: Peer;
}> {
  const broker = createBroker();
  const connecting = RapidClient.connect({
    transport: "local",
    channelFactory: broker.factory,
    authToken: AUTH_TOKEN,
    reconnect: { maxAttempts: 1, backoffMs: 0 },
  });
  const peer = await broker.nextPeer();
  await acceptHello(peer);
  return { client: await connecting, peer };
}

async function createSession(client: RapidClient, peer: Peer) {
  const pending = client.sessions.create({ project: PROJECT });
  const request = await peer.recv();
  assert.equal(request.kind, "request");
  assert.equal(request.method, "sessions.create");
  assert.equal((request.params as { project?: string }).project, PROJECT);
  assert.equal(JSON.stringify(request).includes(AUTH_TOKEN), false);
  peer.send({
    schema: RPC_SCHEMA,
    schema_version: RPC_SCHEMA_VERSION,
    kind: "response",
    id: request.id,
    result: GOLDEN_SESSION,
  });
  return { session: await pending, createRequest: request };
}

function reply(peer: Peer, id: unknown, result: unknown): void {
  peer.send({
    schema: RPC_SCHEMA,
    schema_version: RPC_SCHEMA_VERSION,
    kind: "response",
    id,
    result,
  });
}

function replyError(peer: Peer, id: unknown, error: unknown): void {
  peer.send({
    schema: RPC_SCHEMA,
    schema_version: RPC_SCHEMA_VERSION,
    kind: "response",
    id,
    error,
  });
}

function sendEvent(peer: Peer, id: unknown, event: ReturnType<typeof goldenEvent>): void {
  peer.send({
    schema: RPC_SCHEMA,
    schema_version: RPC_SCHEMA_VERSION,
    kind: "event",
    id,
    cursor: event.seq,
    event,
  });
}

function assertNoCanary(value: unknown, canary: string): void {
  const rendered = `${inspect(value, { depth: 8 })}\n${String(value)}\n${JSON.stringify(value)}`;
  assert.equal(rendered.includes(canary), false, `leaked canary: ${rendered}`);
}

describe("RapidClient session API", () => {
  test("create/run yields a typed Event union that narrows by kind", async () => {
    const { client, peer } = await connectClient();
    assert.equal(inspect(client).includes(AUTH_TOKEN), false);
    const { session } = await createSession(client, peer);
    assert.equal(session.id, SESSION_ID);
    assert.equal(session.seq, 1);
    if (session.snapshot.status === "ready") {
      assert.equal(session.snapshot.top_level_goal, null);
    }

    const kinds: string[] = [];
    const consumed = (async () => {
      for await (const event of session.run({ prompt: "Fix the failing test" })) {
        kinds.push(event.kind);
        if (isEventKind(event, "tool.completed")) {
          const narrowed: "tool.completed" = event.kind;
          assert.equal(narrowed, "tool.completed");
          assert.equal((event.payload as { call_id?: string }).call_id, "call_7");
        }
        if (isEventKind(event, "turn.completed")) {
          const completed: Extract<Event, { kind: "turn.completed" }> = event;
          assert.equal(completed.kind, "turn.completed");
        }
      }
    })();

    const submit = await peer.recv();
    assert.equal(submit.method, "turns.submit");
    assert.deepEqual(
      {
        session_id: (submit.params as { session_id: string }).session_id,
        expected_seq: (submit.params as { expected_seq: number }).expected_seq,
        prompt: (submit.params as { prompt: string }).prompt,
      },
      { session_id: SESSION_ID, expected_seq: 1, prompt: "Fix the failing test" },
    );
    reply(peer, submit.id, { session_id: SESSION_ID, turn_id: TURN_ID, seq: 2 });

    const sub = await peer.recv();
    assert.equal(sub.method, "events.subscribe");
    assert.deepEqual(sub.params, { session_id: SESSION_ID, from_seq: 1 });
    sendEvent(peer, sub.id, goldenEvent(2, "turn.started", { turn_id: TURN_ID }));
    sendEvent(peer, sub.id, goldenEvent(3, "tool.completed", { call_id: "call_7" }));
    sendEvent(peer, sub.id, goldenEvent(4, "turn.completed", { turn_id: TURN_ID }));

    await consumed;
    assert.deepEqual(kinds, ["turn.started", "tool.completed", "turn.completed"]);
    assert.equal(session.seq, 4);

    const cancel = await peer.recv();
    assert.equal(cancel.kind, "cancel");
    assert.equal(cancel.id, sub.id);
    await client.close();
  });

  test("subscribe abort and iterator return close the transport stream", async () => {
    const { client, peer } = await connectClient();
    const { session } = await createSession(client, peer);

    const controller = new AbortController();
    const events: number[] = [];
    const pending = (async () => {
      for await (const event of session.subscribe({ from_seq: 0, signal: controller.signal })) {
        events.push(event.seq);
      }
    })();

    const sub = await peer.recv();
    assert.equal(sub.method, "events.subscribe");
    sendEvent(peer, sub.id, goldenEvent(1));
    await waitUntil(() => events.length === 1);
    controller.abort();
    const cancel = await peer.recv();
    assert.equal(cancel.kind, "cancel");
    assert.equal(cancel.id, sub.id);
    await assert.rejects(
      pending,
      (err: unknown) => err instanceof TransportError && err.code === "cancelled",
    );

    const iterator = session.subscribe({ from_seq: 1 })[Symbol.asyncIterator]();
    const firstPull = iterator.next();
    const sub2 = await peer.recv();
    sendEvent(peer, sub2.id, goldenEvent(2));
    const first = await firstPull;
    assert.equal(first.done, false);
    await iterator.return();
    const cancel2 = await peer.recv();
    assert.equal(cancel2.kind, "cancel");
    assert.equal(cancel2.id, sub2.id);
    await client.close();
  });

  test("backpressure on a live subscribe fails lagged and sends cancel", async () => {
    const { client, peer } = await connectClient();
    const { session } = await createSession(client, peer);
    const iterator = session.subscribe({ from_seq: 0 })[Symbol.asyncIterator]();
    const firstPull = iterator.next();
    const sub = await peer.recv();
    sendEvent(peer, sub.id, goldenEvent(1));
    const first = await firstPull;
    assert.equal(first.value?.seq, 1);

    for (let seq = 2; seq <= MAX_STREAM_BUFFER + 3; seq += 1) {
      sendEvent(peer, sub.id, goldenEvent(seq));
    }
    const cancel = await peer.recv();
    assert.equal(cancel.kind, "cancel");
    assert.equal(cancel.id, sub.id);
    await assert.rejects(async () => {
      for (;;) {
        const next = await iterator.next();
        if (next.done) {
          throw new Error("subscription ended without lag");
        }
      }
    }, (err: unknown) => {
      if (!(err instanceof TransportError)) {
        return false;
      }
      if (err.code === "lagged") {
        return true;
      }
      const cause = err.cause;
      return err.code === "disconnected" && cause instanceof TransportError && cause.code === "lagged";
    });
    await client.close();
  });

  test("refresh re-reads the session so the next run submits at its current seq", async () => {
    const { client, peer } = await connectClient();
    const { session } = await createSession(client, peer);
    const before = session.seq;
    const refreshing = session.refresh();
    const request = await peer.recv();
    assert.equal(request.method, "sessions.get");
    assert.equal((request.params as { session_id: string }).session_id, SESSION_ID);
    reply(peer, request.id, { ...GOLDEN_SESSION, seq: before + 3 });
    await refreshing;
    assert.equal(session.seq, before + 3);
    const other = session.refresh();
    const otherReq = await peer.recv();
    reply(peer, otherReq.id, { ...GOLDEN_SESSION, id: CHILD_ID });
    await assert.rejects(other, (err: unknown) => err instanceof ClientError);
    await client.close();
  });

  test("interrupt/fork/approval map to kernel RPC methods", async () => {
    const { client, peer } = await connectClient();
    const { session } = await createSession(client, peer);

    const interrupting = session.interrupt();
    const interruptReq = await peer.recv();
    assert.equal(interruptReq.method, "turns.interrupt");
    assert.equal((interruptReq.params as { reason: string }).reason, "client_requested");
    reply(peer, interruptReq.id, {});
    await interrupting;

    const forking = session.fork({ at_seq: 1 });
    const forkReq = await peer.recv();
    assert.equal(forkReq.method, "sessions.fork");
    assert.deepEqual(
      {
        source: (forkReq.params as { source: string }).source,
        at_seq: (forkReq.params as { at_seq: number }).at_seq,
      },
      { source: SESSION_ID, at_seq: 1 },
    );
    reply(peer, forkReq.id, { ...GOLDEN_SESSION, id: CHILD_ID, seq: 1 });
    const child = await forking;
    assert.equal(child.id, CHILD_ID);

    const approving = session.approval({ decision: "approved", expected_seq: 1 });
    const approvalReq = await peer.recv();
    assert.equal(approvalReq.method, "approvals.resolve");
    assert.deepEqual(
      {
        session_id: (approvalReq.params as { session_id: string }).session_id,
        expected_seq: (approvalReq.params as { expected_seq: number }).expected_seq,
        decision: (approvalReq.params as { decision: string }).decision,
      },
      { session_id: SESSION_ID, expected_seq: 1, decision: "approved" },
    );
    reply(peer, approvalReq.id, {});
    await approving;
    await client.close();
  });

  test("fails closed on unknown transport, reserved approval fields, and oversized prompt", async () => {
    const src = readFileSync(CLIENT_SRC, "utf8");
    assert.equal(src.includes("node:fs"), false);
    assert.equal(src.includes("node:sqlite"), false);
    assert.equal(/from ["'].*sqlite/i.test(src), false);

    await assert.rejects(
      () =>
        RapidClient.connect({
          transport: "remote" as "local",
          authToken: AUTH_TOKEN,
          channelFactory: () => createMemoryChannelPair().client,
        }),
      (err: unknown) => err instanceof ClientError && err.code === "unsupported_schema",
    );

    const { client, peer } = await connectClient();
    const { session } = await createSession(client, peer);
    await assert.rejects(
      () =>
        session.approval({
          decision: "approved",
          expected_seq: 1,
          grant_capability: SECRET_CANARY,
        } as { decision: "approved"; expected_seq: number }),
      (err: unknown) => {
        assert.ok(err instanceof ClientError);
        assert.equal(err.code, "unknown_variant");
        assertNoCanary(err, SECRET_CANARY);
        return true;
      },
    );
    await assert.rejects(
      () => session.run({ prompt: "x".repeat(16 * 1024 + 1) }).next(),
      (err: unknown) => err instanceof ClientError && err.code === "oversized",
    );
    await assert.rejects(
      () =>
        session.approval({
          decision: "always" as "approved",
          expected_seq: 1,
        }),
      (err: unknown) => err instanceof ClientError && err.code === "unknown_variant",
    );
    await client.close();
  });

  test("policy denial and malformed snapshots fail closed without leaking secrets", async () => {
    const { client, peer } = await connectClient();
    const { session } = await createSession(client, peer);

    const denied = session.approval({ decision: "denied", expected_seq: 1 });
    const approvalReq = await peer.recv();
    replyError(peer, approvalReq.id, PROTOCOL_ERROR);
    await assert.rejects(denied, (err: unknown) => {
      assert.ok(err instanceof RpcError);
      assert.equal(err.error.code, "policy.denied");
      assert.equal(err.error.retryable, false);
      return true;
    });

    const creating = client.sessions.create({ project: PROJECT });
    const createReq = await peer.recv();
    reply(peer, createReq.id, { ...GOLDEN_SESSION, status: "invented", extra: true });
    await assert.rejects(creating, (err: unknown) => err instanceof Error);

    const secretCreate = client.sessions.create({ project: PROJECT });
    const secretReq = await peer.recv();
    replyError(peer, secretReq.id, {
      ...PROTOCOL_ERROR,
      details: { cause: SECRET_CANARY },
    });
    await assert.rejects(secretCreate, (err: unknown) => {
      assert.ok(err instanceof TransportError);
      assertNoCanary(err, SECRET_CANARY);
      return true;
    });
    await client.close();
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
