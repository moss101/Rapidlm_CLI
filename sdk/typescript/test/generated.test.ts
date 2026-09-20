import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { describe, test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  AGENT_SPEC_SCHEMA,
  DecodeError,
  ERROR_CODES,
  ERROR_RETRYABLE,
  EVENT_FAMILY_BY_KIND,
  EVENT_KINDS,
  MAX_DECODE_BYTES,
  MAX_EVENT_BYTES,
  SESSION_STATUSES,
  TOOL_NAMES,
  WIRE_SCHEMA_SHA256,
  decodeAgent,
  decodeAgentResult,
  decodeAgentSpec,
  decodeApiError,
  decodeArtifact,
  decodeEvent,
  decodeGoal,
  decodeSession,
  decodeTool,
  decodeToolResult,
  encodeAgent,
  encodeAgentResult,
  encodeAgentSpec,
  encodeApiError,
  encodeEvent,
  encodeGoal,
  encodeSession,
  encodeTool,
  eventFamily,
  parseJson,
} from "../src/generated/index.ts";

const SDK_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO_ROOT = join(SDK_ROOT, "..", "..");
const SCHEMA_PATH = join(SDK_ROOT, "schemas", "wire.v1.json");
const GENERATED_PATH = join(SDK_ROOT, "src", "generated", "index.ts");
const GENERATE_SCRIPT = join(SDK_ROOT, "scripts", "generate-wire-types.mjs");

const GOLDEN_UUID = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
const PARENT_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789ac";
const VIEW_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
const EVIDENCE_ID = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
const ARTIFACT_ID =
  "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

const PROTOCOL_ERROR = JSON.parse(
  readFileSync(
    join(REPO_ROOT, "crates/protocol/tests/fixtures/error/v1/api_error.json"),
    "utf8",
  ),
);
const PROTOCOL_ARTIFACT = JSON.parse(
  readFileSync(
    join(REPO_ROOT, "crates/protocol/tests/fixtures/artifact/v1/artifact_ref.json"),
    "utf8",
  ),
);
const PROTOCOL_SESSION_ID = JSON.parse(
  readFileSync(
    join(REPO_ROOT, "crates/protocol/tests/fixtures/id/v1/session_id.json"),
    "utf8",
  ),
);

const GOLDEN_AGENT_SPEC = {
  schema: "rapidlm.agent_spec",
  schema_version: 1,
  id: GOLDEN_UUID,
  parent_id: PARENT_ID,
  role: "coder",
  task: "review auth crate",
  workspace_view_id: VIEW_ID,
  model_policy: "balanced",
  budget: {
    max_tokens: 100000,
    max_cost: 2500000,
    max_active_ms: 600000,
    max_tool_calls: 40,
  },
  permissions_profile: "default",
};

const GOLDEN_AGENT_RESULT = {
  schema: "rapidlm.agent_result",
  schema_version: 1,
  agent_id: GOLDEN_UUID,
  status: "succeeded",
  summary: "reviewed auth crate",
  evidence: [EVIDENCE_ID],
  workspace_view: VIEW_ID,
  patch_summary: { files_changed: 1, additions: 12, deletions: 3 },
  artifacts: [PROTOCOL_ARTIFACT],
};

const GOLDEN_GOAL = {
  schema: "rapidlm.goal_snapshot",
  schema_version: 1,
  id: GOLDEN_UUID,
  statement: "ship auth",
  completion_criteria: [{ id: "c1", text: "tests pass" }],
  state: "active",
  stop_reason: null,
  budget: {
    max_turns: 10,
    max_tokens: 100000,
    max_active_ms: null,
    max_cost: null,
  },
  usage: { turns: 0, tokens: 0, active_ms: 0, cost: 0 },
  evidence_requirements: [{ criterion_id: "c1", kinds: ["test"] }],
};

const GOLDEN_EVENT = {
  schema: 1,
  event_id: GOLDEN_UUID,
  session_id: GOLDEN_UUID,
  seq: 42,
  recorded_at: "2026-08-14T15:20:04.123Z",
  actor: { kind: "agent", id: GOLDEN_UUID },
  trace_id: GOLDEN_UUID,
  kind: "tool.completed",
  redaction: "project",
  payload: { call_id: "call_7" },
};

const GOLDEN_SESSION = {
  schema: 1,
  id: GOLDEN_UUID,
  project_id: PARENT_ID,
  status: "ready",
  active_turn: null,
  top_level_goal: {
    id: GOLDEN_UUID,
    statement: "ship auth",
    completion_criteria: [{ id: "c1", text: "tests pass" }],
    state: "active",
    stop_reason: null,
    budget: { max_turns: 10, max_tokens: 100000 },
    usage: { turns: 0, tokens: 0 },
    evidence_requirements: [{ criterion_id: "c1", kinds: ["test"] }],
  },
  active_agents: [GOLDEN_UUID],
  seq: 1,
  created_at: "2026-08-14T15:20:04Z",
  updated_at: "2026-08-14T15:20:04Z",
};

const GOLDEN_TOOL = {
  schema: 1,
  call_id: "call_7",
  tool: "repo.search",
  arguments: { query: "CapabilityLease", repos: ["main"], limit: 20 },
};

const SECRET_CANARY = "hunter2-capability-lease";

function expectDecodeError(fn: () => unknown, code: string): DecodeError {
  try {
    fn();
  } catch (err) {
    assert.ok(err instanceof DecodeError, `expected DecodeError, got ${err}`);
    assert.equal(err.code, code);
    const rendered = `${err.name}: ${err.message}\n${err.path}\n${String(err)}`;
    assert.equal(rendered.includes(SECRET_CANARY), false, `error leaked canary: ${rendered}`);
    assert.equal(rendered.includes("hunter2"), false, `error leaked canary: ${rendered}`);
    return err;
  }
  assert.fail("expected decode to fail");
}

describe("generated wire types", () => {
  test("generation is deterministic and matches the checked-in file", () => {
    const first = spawnSync(process.execPath, [GENERATE_SCRIPT], {
      encoding: "utf8",
    });
    assert.equal(first.status, 0, first.stderr);
    const once = readFileSync(GENERATED_PATH, "utf8");
    const second = spawnSync(process.execPath, [GENERATE_SCRIPT], {
      encoding: "utf8",
    });
    assert.equal(second.status, 0, second.stderr);
    const twice = readFileSync(GENERATED_PATH, "utf8");
    assert.equal(twice, once);

    const check = spawnSync(process.execPath, [GENERATE_SCRIPT, "--check"], {
      encoding: "utf8",
    });
    assert.equal(check.status, 0, check.stderr);

    const catalog = JSON.parse(readFileSync(SCHEMA_PATH, "utf8"));
    const hash = createHash("sha256")
      .update(`${JSON.stringify(catalog, null, 2)}\n`, "utf8")
      .digest("hex");
    assert.equal(WIRE_SCHEMA_SHA256, hash);
    assert.match(once, new RegExp(`schema_sha256:${hash}`));
  });

  test("CI check fails when generated output differs from schemas", () => {
    const original = readFileSync(GENERATED_PATH, "utf8");
    try {
      writeFileSync(GENERATED_PATH, original.replace("rapidlm.sdk.wire", "drifted"));
      const drifted = spawnSync(process.execPath, [GENERATE_SCRIPT, "--check"], {
        encoding: "utf8",
      });
      assert.equal(drifted.status, 1, drifted.stderr);
      assert.match(drifted.stderr, /differ from schemas/);
    } finally {
      writeFileSync(GENERATED_PATH, original);
    }
  });

  test("discriminated unions cover Event, Error, Session, Agent, Goal, Tool", () => {
    assert.ok(EVENT_KINDS.includes("tool.completed"));
    assert.ok(EVENT_KINDS.includes("session.created"));
    assert.equal(EVENT_KINDS.length, 107);
    assert.equal(ERROR_CODES.length, 26);
    assert.equal(SESSION_STATUSES.length, 5);
    assert.equal(TOOL_NAMES.length, 12);
    assert.equal(eventFamily("tool.completed"), "tool");
    assert.equal(eventFamily("goal.created"), "goal");
    assert.equal(ERROR_RETRYABLE["policy.denied"], false);
    assert.equal(ERROR_RETRYABLE["provider.rate_limited"], true);
  });

  test("decodes Rust protocol fixtures", () => {
    const err = decodeApiError(PROTOCOL_ERROR);
    assert.equal(err.code, "policy.denied");
    assert.equal(err.retryable, false);
    assert.equal(err.trace_id, PROTOCOL_SESSION_ID);
    assert.equal(Object.getPrototypeOf(err.details), null);
    assert.deepEqual(encodeApiError(err), PROTOCOL_ERROR);

    const artifact = decodeArtifact(PROTOCOL_ARTIFACT);
    assert.equal(artifact.id, ARTIFACT_ID);
    assert.equal(artifact.redaction, "public");
    assert.equal(artifact.bytes, 3);
  });

  test("decodes Event, Session, Agent, Goal, and Tool records", () => {
    const event = decodeEvent(GOLDEN_EVENT);
    assert.equal(event.kind, "tool.completed");
    if (event.kind === "tool.completed") {
      assert.equal(event.schema, 1);
      assert.equal((event.payload as { call_id: string }).call_id, "call_7");
    }
    assert.deepEqual(encodeEvent(event), GOLDEN_EVENT);

    const session = decodeSession(GOLDEN_SESSION);
    assert.equal(session.status, "ready");
    if (session.status === "ready") {
      assert.equal(session.top_level_goal?.state, "active");
    }
    assert.deepEqual(encodeSession(session), GOLDEN_SESSION);

    const spec = decodeAgentSpec(GOLDEN_AGENT_SPEC);
    assert.equal(spec.role, "coder");
    assert.equal(spec.schema, AGENT_SPEC_SCHEMA);
    assert.deepEqual(encodeAgentSpec(spec), GOLDEN_AGENT_SPEC);

    const result = decodeAgentResult(GOLDEN_AGENT_RESULT);
    assert.equal(result.status, "succeeded");
    assert.deepEqual(encodeAgentResult(result), GOLDEN_AGENT_RESULT);

    const agent = decodeAgent({
      spec: GOLDEN_AGENT_SPEC,
      state: "succeeded",
      stats: { tokens: 1, cost: 2, active_ms: 3, tool_calls: 4 },
      result: GOLDEN_AGENT_RESULT,
    });
    assert.equal(agent.state, "succeeded");
    if (agent.state === "succeeded") {
      assert.equal(agent.result.status, "succeeded");
    }
    assert.deepEqual(
      encodeAgent(agent),
      {
        spec: GOLDEN_AGENT_SPEC,
        state: "succeeded",
        stats: { tokens: 1, cost: 2, active_ms: 3, tool_calls: 4 },
        result: GOLDEN_AGENT_RESULT,
      },
    );

    const goal = decodeGoal(GOLDEN_GOAL);
    assert.equal(goal.state, "active");
    if (goal.state === "active") {
      assert.equal(goal.statement, "ship auth");
    }
    assert.deepEqual(encodeGoal(goal), GOLDEN_GOAL);

    const tool = decodeTool(GOLDEN_TOOL);
    assert.equal(tool.tool, "repo.search");
    if (tool.tool === "repo.search") {
      assert.equal(tool.arguments.query, "CapabilityLease");
    }
    assert.deepEqual(encodeTool(tool), GOLDEN_TOOL);

    const toolResult = decodeToolResult({
      schema: 1,
      call_id: "call_7",
      status: "ok",
      summary: "12 matches",
      data: {},
      artifacts: [],
      truncated: false,
      continuation: null,
    });
    assert.equal(toolResult.status, "ok");
  });

  test("fails closed on unknown variants, schema mismatch, and reserved fields", () => {
    expectDecodeError(
      () => decodeApiError({ ...PROTOCOL_ERROR, code: "policy.allow" }),
      "unknown_variant",
    );
    expectDecodeError(
      () => decodeApiError({ ...PROTOCOL_ERROR, details: { cause: "x" } }),
      "reserved_field",
    );
    expectDecodeError(
      () => decodeApiError({ ...PROTOCOL_ERROR, retryable: true }),
      "malformed",
    );
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT, schema: 2 }),
      "unsupported_schema",
    );
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT, kind: "tool.invented" }),
      "unknown_variant",
    );
    expectDecodeError(
      () => decodeSession({ ...GOLDEN_SESSION, extra: true }),
      "malformed",
    );
    expectDecodeError(
      () => decodeGoal({ ...GOLDEN_GOAL, schema: "other" }),
      "unsupported_schema",
    );
    expectDecodeError(
      () => decodeTool({ ...GOLDEN_TOOL, tool: "fs.write" }),
      "unknown_variant",
    );
    expectDecodeError(
      () =>
        decodeTool({
          ...GOLDEN_TOOL,
          arguments: { query: "x", secret: "hunter2" },
        }),
      "reserved_field",
    );
    expectDecodeError(
      () => decodeAgent({ spec: GOLDEN_AGENT_SPEC, state: "running", stats: { tokens: 0, cost: 0, active_ms: 0, tool_calls: 0 }, result: GOLDEN_AGENT_RESULT }),
      "malformed",
    );
  });

  test("bounds, cancellation, and oversized JSON fail closed", () => {
    const controller = new AbortController();
    controller.abort();
    expectDecodeError(() => decodeEvent(GOLDEN_EVENT, { signal: controller.signal }), "cancelled");

    expectDecodeError(
      () => decodeApiError({ ...PROTOCOL_ERROR, message: "m".repeat(513) }),
      "oversized",
    );

    const nested: { child?: unknown } = {};
    let cursor: { child?: unknown } = nested;
    for (let i = 0; i < 20; i += 1) {
      const next = {};
      cursor.child = next;
      cursor = next;
    }
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT, payload: nested }),
      "oversized",
    );

    expectDecodeError(() => parseJson("x".repeat(MAX_OVERSIZE), { maxBytes: 8 }), "oversized");
    expectDecodeError(() => parseJson("{"), "malformed");
    expectDecodeError(
      () => decodeEvent({ ...GOLDEN_EVENT, recorded_at: "2026-08-14T15:20:04+01:00" }),
      "malformed",
    );
    expectDecodeError(
      () => decodeArtifact({ ...PROTOCOL_ARTIFACT, id: "sha256:deadbeef" }),
      "malformed",
    );
    expectDecodeError(() => decodeEvent(GOLDEN_EVENT, { maxBytes: 8 }), "oversized");
    expectDecodeError(() => decodeTool(GOLDEN_TOOL, { maxBytes: 8 }), "oversized");
    expectDecodeError(
      () => parseJson("x".repeat(MAX_DECODE_BYTES + 1)),
      "oversized",
    );
  });

  test("denies nested/case-folded secrets, proto keys, and oversized payload strings", () => {
    const secretCase = expectDecodeError(
      () =>
        decodeTool({
          ...GOLDEN_TOOL,
          arguments: { query: "x", Secret: SECRET_CANARY },
        }),
      "reserved_field",
    );
    assert.equal(JSON.stringify(secretCase).includes(SECRET_CANARY), false);

    expectDecodeError(
      () =>
        decodeTool({
          ...GOLDEN_TOOL,
          arguments: { query: "x", Capability_Lease: SECRET_CANARY },
        }),
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
            arguments: { secret_plaintext: SECRET_CANARY },
          },
        }),
      "reserved_field",
    );

    expectDecodeError(
      () =>
        decodeTool({
          schema: 1,
          call_id: "call_7",
          tool: "external.call",
          arguments: {
            kind: "plugin",
            plugin: "ext",
            tool: "run",
            arguments: { Capability_Lease: SECRET_CANARY },
          },
        }),
      "reserved_field",
    );

    const protoPayload = JSON.parse(
      `{"schema":1,"call_id":"call_7","tool":"repo.search","arguments":{"query":"x","__proto__":{"secret":"${SECRET_CANARY}"}}}`,
    );
    expectDecodeError(() => decodeTool(protoPayload), "malformed");

    const constructorPayload = JSON.parse(
      `{"schema":1,"call_id":"call_7","tool":"repo.search","arguments":{"query":"x","constructor":{"prototype":{"secret":"${SECRET_CANARY}"}}}}`,
    );
    expectDecodeError(() => decodeTool(constructorPayload), "malformed");

    const prototypePayload = JSON.parse(
      `{"schema":1,"call_id":"call_7","tool":"repo.search","arguments":{"query":"x","prototype":{"secret":"${SECRET_CANARY}"}}}`,
    );
    expectDecodeError(() => decodeTool(prototypePayload), "malformed");

    const oversizedPayload = {
      ...GOLDEN_EVENT,
      payload: { blob: "x".repeat(MAX_EVENT_BYTES + 1) },
    };
    expectDecodeError(() => decodeEvent(oversizedPayload), "oversized");
  });
});

const MAX_OVERSIZE = 16;
