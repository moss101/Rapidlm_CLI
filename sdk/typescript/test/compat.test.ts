import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { inspect } from "node:util";

import {
  DecodeError,
  ERROR_CODES,
  EVENT_KINDS,
  MAX_EVENT_BYTES,
  WIRE_CATALOG_SCHEMA,
  WIRE_SCHEMA_SHA256,
  WIRE_SCHEMA_VERSION,
  decodeApiError,
  decodeEvent,
  decodeSession,
  decodeTool,
  encodeApiError,
  encodeEvent,
  encodeSession,
} from "../src/generated/index.ts";

const SDK_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO_ROOT = join(SDK_ROOT, "..", "..");

const GOLDEN_UUID = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
const SECRET_CANARY = "hunter2-capability-lease";

const PROTOCOL_ERROR = JSON.parse(
  readFileSync(
    join(REPO_ROOT, "crates/protocol/tests/fixtures/error/v1/api_error.json"),
    "utf8",
  ),
) as Record<string, unknown>;

const GOLDEN_SESSION = {
  schema: 1,
  id: GOLDEN_UUID,
  project_id: "018f3c8a-7e2b-7a10-8c4d-0123456789ac",
  status: "ready",
  active_turn: null,
  top_level_goal: null,
  active_agents: [],
  seq: 1,
  created_at: "2026-08-14T15:20:04Z",
  updated_at: "2026-08-14T15:20:04Z",
} as const;

function goldenEvent(
  seq: number,
  kind: string,
  payload: Record<string, unknown> = { call_id: "call_7" },
) {
  return {
    schema: 1,
    event_id: GOLDEN_UUID,
    session_id: GOLDEN_UUID,
    seq,
    recorded_at: "2026-08-14T15:20:04.123Z",
    actor: { kind: "agent", id: GOLDEN_UUID },
    trace_id: GOLDEN_UUID,
    kind,
    redaction: "project",
    payload,
  };
}

const GOLDEN_EVENT_STREAM = [
  goldenEvent(1, "turn.started", { turn_id: GOLDEN_UUID }),
  goldenEvent(2, "tool.requested", { call_id: "call_7", tool: "repo.search" }),
  goldenEvent(3, "tool.completed", { call_id: "call_7" }),
  goldenEvent(4, "turn.completed", { turn_id: GOLDEN_UUID }),
];

function expectDecodeError(fn: () => unknown, code: string): DecodeError {
  try {
    fn();
  } catch (err) {
    assert.ok(err instanceof DecodeError, `expected DecodeError, got ${err}`);
    assert.equal(err.code, code);
    const rendered = `${err.name}: ${err.message}\n${err.path}\n${inspect(err, { depth: 6 })}`;
    assert.equal(rendered.includes(SECRET_CANARY), false, `error leaked canary: ${rendered}`);
    return err;
  }
  assert.fail("expected decode to fail");
}

describe("SDK wire compatibility fixtures", () => {
  test("negotiated schema version matches generated catalog", () => {
    assert.equal(WIRE_CATALOG_SCHEMA, "rapidlm.sdk.wire");
    assert.equal(WIRE_SCHEMA_VERSION, 1);
    assert.match(WIRE_SCHEMA_SHA256, /^[0-9a-f]{64}$/);
    assert.ok(EVENT_KINDS.includes("tool.completed"));
    assert.ok(EVENT_KINDS.includes("turn.started"));
    assert.ok(ERROR_CODES.includes("policy.denied"));
  });

  test("event stream and error goldens round-trip", () => {
    const kinds: string[] = [];
    for (const fixture of GOLDEN_EVENT_STREAM) {
      const event = decodeEvent(fixture);
      kinds.push(event.kind);
      assert.equal(event.schema, 1);
      assert.equal(event.session_id, GOLDEN_UUID);
      assert.deepEqual(encodeEvent(event), fixture);
    }
    assert.deepEqual(kinds, [
      "turn.started",
      "tool.requested",
      "tool.completed",
      "turn.completed",
    ]);

    const session = decodeSession(GOLDEN_SESSION);
    assert.equal(session.status, "ready");
    assert.deepEqual(encodeSession(session), GOLDEN_SESSION);

    const err = decodeApiError(PROTOCOL_ERROR);
    assert.equal(err.code, "policy.denied");
    assert.equal(err.retryable, false);
    assert.deepEqual(encodeApiError(err), PROTOCOL_ERROR);
  });

  test("unknown fields and variants fail closed without leaking secrets", () => {
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT_STREAM[2], extra: true }),
      "malformed",
    );
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT_STREAM[2], kind: "tool.invented" }),
      "unknown_variant",
    );
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT_STREAM[2], schema: 2 }),
      "unsupported_schema",
    );
    expectDecodeError(
      () => decodeSession({ ...GOLDEN_SESSION, extra: SECRET_CANARY }),
      "malformed",
    );
    expectDecodeError(
      () => decodeApiError({ ...PROTOCOL_ERROR, details: { cause: SECRET_CANARY } }),
      "reserved_field",
    );
    expectDecodeError(
      () =>
        decodeTool({
          schema: 1,
          call_id: "call_7",
          tool: "external.call",
          arguments: {
            kind: "mcp",
            server: "docs",
            tool: "search",
            arguments: { grant_capability: SECRET_CANARY },
          },
        }),
      "reserved_field",
    );
    expectDecodeError(
      () =>
        decodeTool({
          schema: 1,
          call_id: "call_7",
          tool: "fs.write",
          arguments: {},
        }),
      "unknown_variant",
    );
  });

  test("cancellation and oversized frames fail closed", () => {
    const controller = new AbortController();
    controller.abort();
    expectDecodeError(
      () => decodeEvent(GOLDEN_EVENT_STREAM[0], { signal: controller.signal }),
      "cancelled",
    );
    expectDecodeError(
      () => decodeEvent(GOLDEN_EVENT_STREAM[0], { maxBytes: 8 }),
      "oversized",
    );
    expectDecodeError(() => decodeApiError({ ...PROTOCOL_ERROR, message: "m".repeat(513) }), "oversized");
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT_STREAM[2], payload: { blob: "x".repeat(MAX_EVENT_BYTES + 1) } }),
      "oversized",
    );
  });
});
