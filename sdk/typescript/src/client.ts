// Public session client over the local SDK transport.
// Maps 1:1 onto kernel RPC methods. Prompt/project strings are untrusted data.

import {
  ACTOR_KINDS,
  DecodeError,
  MAX_ACTOR_ATTR_BYTES,
  MAX_TASK_BYTES,
  UUID_PATTERN,
  decodeSession,
} from "./generated/index.ts";
import type {
  ActorRef,
  Event,
  EventKind,
  JsonObject,
  JsonValue,
  Session as SessionRecord,
  Uuid,
} from "./generated/index.ts";
import {
  LocalTransport,
  TransportError,
} from "./transport/local.ts";
import type { LocalConnectOptions, RequestOptions } from "./transport/local.ts";

export const MAX_PROJECT_PATH_BYTES = 4096 as const;
export const MAX_PROMPT_BYTES = MAX_TASK_BYTES;
export const INTERRUPT_REASON = "client_requested" as const;
export const APPROVAL_DECISIONS = ["approved", "denied"] as const;

const CONNECT_FIELDS = [
  "transport",
  "socketPath",
  "websocketUrl",
  "authToken",
  "channelFactory",
  "signal",
  "handshakeTimeoutMs",
  "requestTimeoutMs",
  "reconnect",
] as const;

const CREATE_FIELDS = ["project", "actor", "trace_id", "signal"] as const;
const RUN_FIELDS = ["prompt", "expected_seq", "actor", "trace_id", "signal"] as const;
const SUBSCRIBE_FIELDS = ["from_seq", "signal"] as const;
const INTERRUPT_FIELDS = ["reason", "actor", "trace_id", "signal"] as const;
const FORK_FIELDS = ["at_seq", "actor", "trace_id", "signal"] as const;
const APPROVAL_FIELDS = ["decision", "expected_seq", "actor", "trace_id", "signal"] as const;
const ACTOR_FIELDS = ["kind", "id", "org_id", "device_id"] as const;
const TURN_HANDLE_FIELDS = ["session_id", "turn_id", "seq"] as const;
const PROTOTYPE_KEYS = new Set(["__proto__", "constructor", "prototype"]);
const RUN_TERMINAL_KINDS = new Set<EventKind>([
  "turn.completed",
  "turn.failed",
  "turn.interrupted",
  "session.closed",
]);

export type ClientErrorCode =
  | "cancelled"
  | "unsupported_schema"
  | "oversized"
  | "malformed"
  | "unknown_variant"
  | "protocol";

export type ApprovalDecision = (typeof APPROVAL_DECISIONS)[number];

export type RapidClientConnectOptions = LocalConnectOptions & {
  transport: "local";
};

export type CreateSessionRequest = {
  project: string;
  actor?: ActorRef;
  trace_id?: Uuid;
  signal?: AbortSignal;
};

export type RunRequest = {
  prompt: string;
  expected_seq?: number;
  actor?: ActorRef;
  trace_id?: Uuid;
  signal?: AbortSignal;
};

export type SubscribeRequest = {
  from_seq?: number;
  signal?: AbortSignal;
};

export type InterruptRequest = {
  reason?: typeof INTERRUPT_REASON;
  actor?: ActorRef;
  trace_id?: Uuid;
  signal?: AbortSignal;
};

export type ForkRequest = {
  at_seq?: number;
  actor?: ActorRef;
  trace_id?: Uuid;
  signal?: AbortSignal;
};

export type ApprovalRequest = {
  decision: ApprovalDecision;
  expected_seq: number;
  actor?: ActorRef;
  trace_id?: Uuid;
  signal?: AbortSignal;
};

export type TurnHandle = {
  session_id: Uuid;
  turn_id: Uuid;
  seq: number;
};

type _EventNarrows<K extends EventKind> = Extract<Event, { kind: K }> extends {
  kind: K;
}
  ? true
  : never;
type _AssertEventNarrowing = _EventNarrows<"tool.completed"> &
  _EventNarrows<"approval.requested"> &
  _EventNarrows<"turn.completed">;
const _eventNarrowingOk: _AssertEventNarrowing = true;
void _eventNarrowingOk;

export class ClientError extends Error {
  readonly code: ClientErrorCode;

  constructor(code: ClientErrorCode, message: string, extras?: { cause?: unknown }) {
    super(message, extras?.cause === undefined ? undefined : { cause: extras.cause });
    this.name = "ClientError";
    this.code = code;
  }
}

export function isEventKind<K extends EventKind>(
  event: Event,
  kind: K,
): event is Extract<Event, { kind: K }> {
  return event.kind === kind;
}

export class RapidClient {
  readonly #transport: LocalTransport;
  readonly sessions: {
    create: (request: CreateSessionRequest) => Promise<Session>;
  };

  private constructor(transport: LocalTransport) {
    this.#transport = transport;
    this.sessions = {
      create: (request) => this.#createSession(request),
    };
  }

  static async connect(options: RapidClientConnectOptions): Promise<RapidClient> {
    rejectUnknownKeys(options, CONNECT_FIELDS, "connect");
    if (options.transport !== "local") {
      throw new ClientError("unsupported_schema", "only local transport is supported");
    }
    const local: LocalConnectOptions = {};
    if (options.socketPath !== undefined) {
      local.socketPath = options.socketPath;
    }
    if (options.websocketUrl !== undefined) {
      local.websocketUrl = options.websocketUrl;
    }
    if (options.authToken !== undefined) {
      local.authToken = options.authToken;
    }
    if (options.channelFactory !== undefined) {
      local.channelFactory = options.channelFactory;
    }
    if (options.signal !== undefined) {
      local.signal = options.signal;
    }
    if (options.handshakeTimeoutMs !== undefined) {
      local.handshakeTimeoutMs = options.handshakeTimeoutMs;
    }
    if (options.requestTimeoutMs !== undefined) {
      local.requestTimeoutMs = options.requestTimeoutMs;
    }
    if (options.reconnect !== undefined) {
      local.reconnect = options.reconnect;
    }
    const transport = await LocalTransport.connect(local);
    return new RapidClient(transport);
  }

  get connected(): boolean {
    return this.#transport.connected;
  }

  async close(): Promise<void> {
    await this.#transport.close();
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `RapidClient { connected: ${this.#transport.connected} }`;
  }

  async #createSession(request: CreateSessionRequest): Promise<Session> {
    rejectUnknownKeys(request, CREATE_FIELDS, "sessions.create");
    const project = requireProject(request.project);
    const signal = request.signal;
    const params: JsonObject = {
      project,
      trace_id: resolveTraceId(request.trace_id),
    };
    const actor = encodeActor(request.actor);
    if (actor !== undefined) {
      params.actor = actor;
    }
    const raw = await this.#transport.request("sessions.create", params, requestOptions(signal));
    const snapshot = decodeSession(raw, signal === undefined ? undefined : { signal });
    return Session.fromSnapshot(this.#transport, snapshot);
  }
}

export class Session {
  readonly #transport: LocalTransport;
  #snapshot: SessionRecord;

  private constructor(transport: LocalTransport, snapshot: SessionRecord) {
    this.#transport = transport;
    this.#snapshot = snapshot;
  }

  static fromSnapshot(transport: LocalTransport, snapshot: SessionRecord): Session {
    return new Session(transport, snapshot);
  }

  get id(): Uuid {
    return this.#snapshot.id;
  }

  get seq(): number {
    return this.#snapshot.seq;
  }

  get snapshot(): SessionRecord {
    return this.#snapshot;
  }

  async *run(request: RunRequest): AsyncGenerator<Event, void, void> {
    rejectUnknownKeys(request, RUN_FIELDS, "run");
    const prompt = requirePrompt(request.prompt);
    const expectedSeq =
      request.expected_seq === undefined ? this.#snapshot.seq : asUint(request.expected_seq, "expected_seq");
    const params: JsonObject = {
      session_id: this.#snapshot.id,
      expected_seq: expectedSeq,
      prompt,
      trace_id: resolveTraceId(request.trace_id),
    };
    const actor = encodeActor(request.actor);
    if (actor !== undefined) {
      params.actor = actor;
    }

    const stream = new StreamLease(request.signal);
    let iterator: AsyncIterator<Event> | undefined;
    try {
      const handle = decodeTurnHandle(
        await this.#transport.request("turns.submit", params, requestOptions(stream.signal)),
      );
      if (handle.session_id !== this.#snapshot.id) {
        throw new ClientError("protocol", "turn handle session_id does not match session");
      }
      this.#noteSeq(handle.seq);

      const fromSeq = handle.seq === 0 ? 0 : handle.seq - 1;
      iterator = this.#events(fromSeq, stream.signal)[Symbol.asyncIterator]();
      for (;;) {
        const next = await iterator.next();
        if (next.done) {
          return;
        }
        yield next.value;
        if (isRunTerminal(next.value, handle.turn_id)) {
          return;
        }
      }
    } finally {
      await closeStream(stream, iterator);
    }
  }

  async *subscribe(request: SubscribeRequest = {}): AsyncGenerator<Event, void, void> {
    rejectUnknownKeys(request, SUBSCRIBE_FIELDS, "subscribe");
    const fromSeq =
      request.from_seq === undefined ? this.#snapshot.seq : asUint(request.from_seq, "from_seq");
    const stream = new StreamLease(request.signal);
    const iterator = this.#events(fromSeq, stream.signal)[Symbol.asyncIterator]();
    try {
      for (;;) {
        const next = await iterator.next();
        if (next.done) {
          return;
        }
        yield next.value;
      }
    } finally {
      await closeStream(stream, iterator);
    }
  }

  async interrupt(request: InterruptRequest = {}): Promise<void> {
    rejectUnknownKeys(request, INTERRUPT_FIELDS, "interrupt");
    if (request.reason !== undefined && request.reason !== INTERRUPT_REASON) {
      throw new ClientError("unknown_variant", "interrupt reason is not supported");
    }
    const params: JsonObject = {
      session_id: this.#snapshot.id,
      reason: INTERRUPT_REASON,
      trace_id: resolveTraceId(request.trace_id),
    };
    const actor = encodeActor(request.actor);
    if (actor !== undefined) {
      params.actor = actor;
    }
    await this.#transport.request("turns.interrupt", params, requestOptions(request.signal));
  }

  async fork(request: ForkRequest = {}): Promise<Session> {
    rejectUnknownKeys(request, FORK_FIELDS, "fork");
    const atSeq = request.at_seq === undefined ? this.#snapshot.seq : asUint(request.at_seq, "at_seq");
    if (atSeq === 0) {
      throw new ClientError("malformed", "fork at_seq must be a committed sequence");
    }
    const params: JsonObject = {
      source: this.#snapshot.id,
      at_seq: atSeq,
      trace_id: resolveTraceId(request.trace_id),
    };
    const actor = encodeActor(request.actor);
    if (actor !== undefined) {
      params.actor = actor;
    }
    const raw = await this.#transport.request("sessions.fork", params, requestOptions(request.signal));
    const snapshot = decodeSession(raw, request.signal === undefined ? undefined : { signal: request.signal });
    return Session.fromSnapshot(this.#transport, snapshot);
  }

  async approval(request: ApprovalRequest): Promise<void> {
    rejectUnknownKeys(request, APPROVAL_FIELDS, "approval");
    if (!(APPROVAL_DECISIONS as readonly string[]).includes(request.decision)) {
      throw new ClientError("unknown_variant", "approval decision is not supported");
    }
    const params: JsonObject = {
      session_id: this.#snapshot.id,
      expected_seq: asUint(request.expected_seq, "expected_seq"),
      decision: request.decision,
      trace_id: resolveTraceId(request.trace_id),
    };
    const actor = encodeActor(request.actor);
    if (actor !== undefined) {
      params.actor = actor;
    }
    await this.#transport.request("approvals.resolve", params, requestOptions(request.signal));
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `Session { id: ${this.#snapshot.id}, seq: ${this.#snapshot.seq} }`;
  }

  async *#events(fromSeq: number, signal: AbortSignal): AsyncGenerator<Event, void, void> {
    for await (const event of this.#transport.subscribe(
      { session_id: this.#snapshot.id, from_seq: fromSeq },
      { signal },
    )) {
      if (event.session_id !== this.#snapshot.id) {
        throw new ClientError("protocol", "event session_id does not match session");
      }
      this.#noteSeq(event.seq);
      yield event;
    }
  }

  #noteSeq(seq: number): void {
    if (seq > this.#snapshot.seq) {
      this.#snapshot = { ...this.#snapshot, seq };
    }
  }
}

class StreamLease {
  readonly #controller = new AbortController();
  readonly signal: AbortSignal;

  constructor(parent?: AbortSignal) {
    const signals: AbortSignal[] = [this.#controller.signal];
    if (parent) {
      signals.push(parent);
    }
    this.signal = signals.length === 1 ? this.#controller.signal : AbortSignal.any(signals);
    throwIfAborted(this.signal);
  }

  close(): void {
    if (!this.#controller.signal.aborted) {
      this.#controller.abort();
    }
  }
}

async function closeStream(
  stream: StreamLease,
  iterator: AsyncIterator<Event> | undefined,
): Promise<void> {
  stream.close();
  if (!iterator?.return) {
    return;
  }
  try {
    await iterator.return();
  } catch {
    // Aborting a live subscribe rejects the pending pull; the cancel is already sent.
  }
}

function requestOptions(signal?: AbortSignal): RequestOptions | undefined {
  return signal === undefined ? undefined : { signal };
}

function requireProject(value: unknown): string {
  const project = requireBoundedString(value, "project", MAX_PROJECT_PATH_BYTES);
  if (project.includes("\0") || project.includes("\n") || project.includes("\r")) {
    throw new ClientError("malformed", "project path is invalid");
  }
  return project;
}

function requirePrompt(value: unknown): string {
  return requireBoundedString(value, "prompt", MAX_PROMPT_BYTES);
}

function requireBoundedString(value: unknown, name: string, maxBytes: number): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new ClientError("malformed", `${name} must be a non-empty string`);
  }
  if (Buffer.byteLength(value, "utf8") > maxBytes) {
    throw new ClientError("oversized", `${name} exceeds byte bound`);
  }
  return value;
}

function resolveTraceId(value: string | undefined): string {
  if (value === undefined) {
    return crypto.randomUUID();
  }
  return asUuid(value, "trace_id");
}

function encodeActor(actor: ActorRef | undefined): JsonObject | undefined {
  if (actor === undefined) {
    return undefined;
  }
  rejectUnknownKeys(actor, ACTOR_FIELDS, "actor");
  if (!(ACTOR_KINDS as readonly string[]).includes(actor.kind)) {
    throw new ClientError("unknown_variant", "actor.kind is not supported");
  }
  const encoded: JsonObject = {
    kind: actor.kind,
    id: requireBoundedString(actor.id, "actor.id", MAX_ACTOR_ATTR_BYTES),
  };
  if (actor.org_id !== undefined) {
    encoded.org_id = requireBoundedString(actor.org_id, "actor.org_id", MAX_ACTOR_ATTR_BYTES);
  }
  if (actor.device_id !== undefined) {
    encoded.device_id = requireBoundedString(actor.device_id, "actor.device_id", MAX_ACTOR_ATTR_BYTES);
  }
  return encoded;
}

function decodeTurnHandle(input: JsonValue): TurnHandle {
  if (input === null || typeof input !== "object" || Array.isArray(input)) {
    throw new ClientError("malformed", "turn handle must be an object");
  }
  rejectUnknownKeys(input, TURN_HANDLE_FIELDS, "turn");
  return {
    session_id: asUuid(input.session_id, "session_id"),
    turn_id: asUuid(input.turn_id, "turn_id"),
    seq: asUint(input.seq, "seq"),
  };
}

function isRunTerminal(event: Event, turnId: string): boolean {
  if (!RUN_TERMINAL_KINDS.has(event.kind)) {
    return false;
  }
  if (event.kind === "session.closed") {
    return true;
  }
  if (event.payload !== null && typeof event.payload === "object" && !Array.isArray(event.payload)) {
    const payloadTurn = event.payload.turn_id;
    if (typeof payloadTurn === "string" && payloadTurn !== turnId) {
      return false;
    }
  }
  return true;
}

function rejectUnknownKeys(
  value: object,
  allowed: readonly string[],
  path: string,
): void {
  for (const key of Object.keys(value)) {
    if (PROTOTYPE_KEYS.has(key)) {
      throw new ClientError("malformed", "prototype-polluting field");
    }
    if (!allowed.includes(key)) {
      throw new ClientError("unknown_variant", `unknown field ${path}.${key}`);
    }
  }
}

function asUuid(value: unknown, path: string): string {
  if (typeof value !== "string" || !UUID_PATTERN.test(value)) {
    throw new ClientError("malformed", `${path} must be a lowercase UUID`);
  }
  return value;
}

function asUint(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new ClientError("malformed", `${path} must be an unsigned integer`);
  }
  if (value > Number.MAX_SAFE_INTEGER) {
    throw new ClientError("oversized", `${path} exceeds safe integer bound`);
  }
  return value;
}

function throwIfAborted(signal: AbortSignal): void {
  if (!signal.aborted) {
    return;
  }
  const reason = signal.reason;
  if (reason instanceof ClientError || reason instanceof TransportError || reason instanceof DecodeError) {
    throw reason;
  }
  throw new ClientError("cancelled", "request cancelled", { cause: reason });
}
