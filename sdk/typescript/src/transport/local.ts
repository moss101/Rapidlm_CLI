// Local daemon IPC / loopback websocket transport.
// Request/stream only — no session or policy business rules.
// Never opens a ledger database or repository files.

import net from "node:net";

import {
  DecodeError,
  MAX_DECODE_BYTES,
  MAX_EVENT_BYTES,
  UUID_PATTERN,
  WIRE_CATALOG_SCHEMA,
  WIRE_SCHEMA_SHA256,
  WIRE_SCHEMA_VERSION,
  decodeApiError,
  decodeEvent,
  parseJson,
} from "../generated/index.ts";
import type { ApiError, Event, JsonObject, JsonValue } from "../generated/index.ts";

export const RPC_SCHEMA = "rapidlm.sdk.rpc" as const;
export const RPC_SCHEMA_VERSION = 1 as const;
export const MAX_RPC_FRAME_BYTES = MAX_EVENT_BYTES;
export const MAX_CONTROL_FRAME_BYTES = MAX_DECODE_BYTES;
export const MAX_IN_FLIGHT = 64 as const;
export const MAX_STREAM_BUFFER = 32 as const;
export const MAX_WRITE_BUFFER_BYTES = MAX_DECODE_BYTES * 4;
export const MAX_AUTH_HANDLE_BYTES = 4096 as const;
export const DEFAULT_HANDSHAKE_TIMEOUT_MS = 5_000 as const;
export const DEFAULT_REQUEST_TIMEOUT_MS = 60_000 as const;
export const DEFAULT_RECONNECT_ATTEMPTS = 3 as const;
export const DEFAULT_RECONNECT_BACKOFF_MS = 50 as const;

export const RPC_METHODS = [
  "sessions.create",
  "sessions.get",
  "sessions.fork",
  "sessions.rewind",
  "turns.submit",
  "turns.interrupt",
  "events.subscribe",
  "approvals.resolve",
] as const;

export type RpcMethod = (typeof RPC_METHODS)[number];

export type TransportErrorCode =
  | "cancelled"
  | "disconnected"
  | "protocol"
  | "auth.required"
  | "unsupported_schema"
  | "oversized"
  | "unknown_outcome"
  | "timeout"
  | "lagged";

const TRANSPORT_CODES: readonly TransportErrorCode[] = [
  "cancelled",
  "disconnected",
  "protocol",
  "auth.required",
  "unsupported_schema",
  "oversized",
  "unknown_outcome",
  "timeout",
  "lagged",
];

const PROTOTYPE_KEYS = new Set(["__proto__", "constructor", "prototype"]);

const HELLO_OK_FIELDS = [
  "schema",
  "schema_version",
  "kind",
  "id",
  "wire_schema",
  "wire_schema_version",
  "wire_schema_sha256",
] as const;

const RESPONSE_FIELDS = ["schema", "schema_version", "kind", "id", "result", "error"] as const;
const EVENT_FIELDS = ["schema", "schema_version", "kind", "id", "cursor", "event"] as const;
const STREAM_END_FIELDS = [
  "schema",
  "schema_version",
  "kind",
  "id",
  "cursor",
  "reason",
  "error",
] as const;
const STREAM_END_REASONS = ["complete", "cancelled", "lagged", "disconnected"] as const;

export type StreamEndReason = (typeof STREAM_END_REASONS)[number];

export type SubscribeParams = {
  session_id: string;
  from_seq: number;
};

export type RequestOptions = {
  signal?: AbortSignal;
  timeoutMs?: number;
};

export type SubscribeOptions = {
  signal?: AbortSignal;
};

export type ReconnectPolicy = {
  maxAttempts: number;
  backoffMs: number;
};

export type TransportChannel = {
  send(frame: string): void;
  frames(): AsyncIterable<string>;
  close(): void;
};

export type ChannelFactory = () => TransportChannel | Promise<TransportChannel>;

export type LocalConnectOptions = {
  /** Unix socket or Windows named-pipe path. Never a remote URL. */
  socketPath?: string;
  /** Loopback websocket only (`ws://127.0.0.1` / `ws://[::1]`). */
  websocketUrl?: string;
  /** Opaque local-auth handle. Required. Never logged. */
  authToken?: string;
  /** Injected channel (tests / in-process bridge). */
  channelFactory?: ChannelFactory;
  signal?: AbortSignal;
  handshakeTimeoutMs?: number;
  requestTimeoutMs?: number;
  reconnect?: Partial<ReconnectPolicy>;
};

type PendingUnary = {
  resolve: (value: JsonValue) => void;
  reject: (err: unknown) => void;
};

type PendingStream = {
  queue: BoundedQueue<Event>;
  cursor: number;
};

type HelloOk = {
  kind: "hello_ok";
  id: string;
  wire_schema: string;
  wire_schema_version: number;
  wire_schema_sha256: string | null;
};

type InboundResponse = {
  kind: "response";
  id: string;
  result: JsonValue | undefined;
  errorRaw: unknown | undefined;
};

type InboundEvent = {
  kind: "event";
  id: string;
  cursor: number | undefined;
  eventRaw: unknown;
};

type InboundStreamEnd = {
  kind: "stream_end";
  id: string;
  cursor: number;
  reason: StreamEndReason;
  errorRaw: unknown | undefined;
};

type InboundFrame = HelloOk | InboundResponse | InboundEvent | InboundStreamEnd;

export class TransportError extends Error {
  readonly code: TransportErrorCode;
  readonly resumeCursor: number | null;
  readonly apiError: ApiError | undefined;

  constructor(
    code: TransportErrorCode,
    message: string,
    extras?: { resumeCursor?: number; apiError?: ApiError; cause?: unknown },
  ) {
    super(message, extras?.cause === undefined ? undefined : { cause: extras.cause });
    this.name = "TransportError";
    this.code = code;
    this.resumeCursor = extras?.resumeCursor ?? null;
    this.apiError = extras?.apiError;
  }
}

export class RpcError extends Error {
  readonly error: ApiError;

  constructor(error: ApiError) {
    super(error.message);
    this.name = "RpcError";
    this.error = error;
  }
}

export class LocalTransport {
  readonly #authToken: string;
  readonly #factory: ChannelFactory;
  readonly #lifetime: AbortController;
  readonly #handshakeTimeoutMs: number;
  readonly #requestTimeoutMs: number;
  readonly #reconnect: ReconnectPolicy;
  readonly #pending = new Map<string, PendingUnary>();
  readonly #streams = new Map<string, PendingStream>();

  #channel: TransportChannel | null = null;
  #reader: Promise<void> | null = null;
  #connected = false;
  #connectLock: Promise<void> | null = null;
  #closed = false;

  private constructor(
    authToken: string,
    factory: ChannelFactory,
    handshakeTimeoutMs: number,
    requestTimeoutMs: number,
    reconnect: ReconnectPolicy,
    parentSignal?: AbortSignal,
  ) {
    this.#authToken = authToken;
    this.#factory = factory;
    this.#lifetime = new AbortController();
    this.#handshakeTimeoutMs = handshakeTimeoutMs;
    this.#requestTimeoutMs = requestTimeoutMs;
    this.#reconnect = reconnect;
    if (parentSignal) {
      if (parentSignal.aborted) {
        this.#lifetime.abort(parentSignal.reason);
      } else {
        parentSignal.addEventListener(
          "abort",
          () => {
            void this.close();
          },
          { once: true },
        );
      }
    }
  }

  static async connect(options: LocalConnectOptions): Promise<LocalTransport> {
    const authToken = requireAuthToken(options.authToken);
    const factory = resolveChannelFactory(options);
    const handshakeTimeoutMs = requirePositiveInt(
      options.handshakeTimeoutMs ?? DEFAULT_HANDSHAKE_TIMEOUT_MS,
      "handshakeTimeoutMs",
    );
    const requestTimeoutMs = requirePositiveInt(
      options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS,
      "requestTimeoutMs",
    );
    const reconnect = normalizeReconnect(options.reconnect);
    const transport = new LocalTransport(
      authToken,
      factory,
      handshakeTimeoutMs,
      requestTimeoutMs,
      reconnect,
      options.signal,
    );
    try {
      await transport.#ensureChannel(options.signal);
      return transport;
    } catch (err) {
      await transport.close();
      throw err;
    }
  }

  get connected(): boolean {
    return this.#connected && !this.#closed;
  }

  async request(
    method: RpcMethod,
    params: JsonObject,
    options?: RequestOptions,
  ): Promise<JsonValue> {
    if (method === "events.subscribe") {
      throw new TransportError("protocol", "events.subscribe must use subscribe()");
    }
    if (!RPC_METHODS.includes(method)) {
      throw new TransportError("protocol", "unknown RPC method");
    }
    this.#assertOpen();
    await this.#ensureChannel(options?.signal);
    const timeoutMs = requirePositiveInt(
      options?.timeoutMs ?? this.#requestTimeoutMs,
      "timeoutMs",
    );
    const id = newRequestId();
    if (this.#pending.size + this.#streams.size >= MAX_IN_FLIGHT) {
      throw new TransportError("oversized", "in-flight RPC bound exceeded");
    }

    const timeout = AbortSignal.timeout(timeoutMs);
    const signals: AbortSignal[] = [this.#lifetime.signal, timeout];
    if (options?.signal) {
      signals.push(options.signal);
    }
    const signal = AbortSignal.any(signals);

    return new Promise<JsonValue>((resolve, reject) => {
      const settle = (fn: () => void): void => {
        signal.removeEventListener("abort", onAbort);
        this.#pending.delete(id);
        fn();
      };
      const onAbort = (): void => {
        this.#sendCancel(id);
        const err = abortToError(signal);
        settle(() => reject(err));
      };
      this.#pending.set(id, {
        resolve: (value) => settle(() => resolve(value)),
        reject: (err) => settle(() => reject(err)),
      });
      if (signal.aborted) {
        onAbort();
        return;
      }
      signal.addEventListener("abort", onAbort, { once: true });
      try {
        this.#send({
          schema: RPC_SCHEMA,
          schema_version: RPC_SCHEMA_VERSION,
          kind: "request",
          id,
          method,
          params,
        });
      } catch (err) {
        settle(() => reject(err));
      }
    });
  }

  async *subscribe(
    params: SubscribeParams,
    options?: SubscribeOptions,
  ): AsyncGenerator<Event, void, void> {
    const sessionId = asUuid(params.session_id, "session_id");
    let cursor = asUint(params.from_seq, "from_seq");
    this.#assertOpen();

    const signals: AbortSignal[] = [this.#lifetime.signal];
    if (options?.signal) {
      signals.push(options.signal);
    }
    const signal = AbortSignal.any(signals);
    throwIfAborted(signal);

    let attempts = 0;
    while (!this.#closed) {
      throwIfAborted(signal);
      try {
        await this.#ensureChannel(signal);
        attempts = 0;
        const id = newRequestId();
        if (this.#pending.size + this.#streams.size >= MAX_IN_FLIGHT) {
          throw new TransportError("oversized", "in-flight RPC bound exceeded");
        }
        const queue = new BoundedQueue<Event>(MAX_STREAM_BUFFER);
        this.#streams.set(id, { queue, cursor });
        const onAbort = (): void => {
          this.#sendCancel(id);
          queue.fail(abortToError(signal));
        };
        if (signal.aborted) {
          onAbort();
        } else {
          signal.addEventListener("abort", onAbort, { once: true });
        }
        try {
          this.#send({
            schema: RPC_SCHEMA,
            schema_version: RPC_SCHEMA_VERSION,
            kind: "request",
            id,
            method: "events.subscribe",
            params: { session_id: sessionId, from_seq: cursor },
          });
          for (;;) {
            const next = await queue.next();
            if (next.done) {
              return;
            }
            const event = next.value;
            if (event.seq <= cursor) {
              continue;
            }
            if (event.seq !== cursor + 1) {
              throw new TransportError(
                "protocol",
                "event sequence gap",
                { resumeCursor: cursor },
              );
            }
            cursor = event.seq;
            const stream = this.#streams.get(id);
            if (stream) {
              stream.cursor = cursor;
            }
            yield event;
          }
        } finally {
          signal.removeEventListener("abort", onAbort);
          this.#streams.delete(id);
        }
      } catch (err) {
        if (this.#closed || signal.aborted) {
          throw abortToError(signal);
        }
        if (!isReconnectable(err)) {
          throw err;
        }
        attempts += 1;
        if (attempts >= this.#reconnect.maxAttempts) {
          const resume =
            err instanceof TransportError ? err.resumeCursor : cursor;
          throw new TransportError("disconnected", "event stream reconnect exhausted", {
            resumeCursor: resume ?? cursor,
            cause: err,
          });
        }
        if (this.#reconnect.backoffMs > 0) {
          await sleep(this.#reconnect.backoffMs, signal);
        }
      }
    }
    throw new TransportError("cancelled", "transport closed");
  }

  async close(): Promise<void> {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#connected = false;
    this.#lifetime.abort();
    const cancelled = new TransportError("cancelled", "transport closed");
    this.#failPending(cancelled);
    const channel = this.#channel;
    this.#channel = null;
    channel?.close();
    if (this.#reader) {
      try {
        await this.#reader;
      } catch {
        // Reader exit is expected after close.
      }
      this.#reader = null;
    }
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return "LocalTransport {}";
  }

  async #ensureChannel(signal?: AbortSignal): Promise<void> {
    if (this.#closed) {
      throw new TransportError("cancelled", "transport closed");
    }
    if (this.#connected && this.#channel) {
      return;
    }
    if (this.#connectLock) {
      await this.#connectLock;
      if (this.#connected && this.#channel) {
        return;
      }
    }
    const run = this.#connect(signal);
    this.#connectLock = run;
    try {
      await run;
    } finally {
      if (this.#connectLock === run) {
        this.#connectLock = null;
      }
    }
  }

  async #connect(signal?: AbortSignal): Promise<void> {
    throwIfAborted(signal);
    throwIfAborted(this.#lifetime.signal);
    const previous = this.#channel;
    this.#channel = null;
    this.#connected = false;
    previous?.close();

    const channel = await this.#factory();
    this.#channel = channel;
    this.#reader = this.#readLoop(channel);

    const id = newRequestId();
    const timeout = AbortSignal.timeout(this.#handshakeTimeoutMs);
    const signals: AbortSignal[] = [this.#lifetime.signal, timeout];
    if (signal) {
      signals.push(signal);
    }
    const combined = AbortSignal.any(signals);

    await new Promise<void>((resolve, reject) => {
      const settle = (fn: () => void): void => {
        combined.removeEventListener("abort", onAbort);
        this.#pending.delete(id);
        fn();
      };
      const onAbort = (): void => {
        settle(() => reject(abortToError(combined)));
      };
      this.#pending.set(id, {
        resolve: () => settle(() => resolve()),
        reject: (err) => settle(() => reject(err)),
      });
      if (combined.aborted) {
        onAbort();
        return;
      }
      combined.addEventListener("abort", onAbort, { once: true });
      try {
        this.#send({
          schema: RPC_SCHEMA,
          schema_version: RPC_SCHEMA_VERSION,
          kind: "hello",
          id,
          wire_schema: WIRE_CATALOG_SCHEMA,
          wire_schema_version: WIRE_SCHEMA_VERSION,
          wire_schema_sha256: WIRE_SCHEMA_SHA256,
          auth: { kind: "local", handle: this.#authToken },
        });
      } catch (err) {
        settle(() => reject(err));
      }
    });
    this.#connected = true;
  }

  async #readLoop(channel: TransportChannel): Promise<void> {
    try {
      for await (const line of channel.frames()) {
        if (this.#closed || this.#channel !== channel) {
          return;
        }
        this.#dispatch(line);
      }
      this.#onChannelGone(channel, new TransportError("disconnected", "daemon connection closed"));
    } catch (err) {
      this.#onChannelGone(channel, err);
    }
  }

  #onChannelGone(channel: TransportChannel, err: unknown): void {
    if (this.#channel !== channel) {
      return;
    }
    this.#connected = false;
    this.#channel = null;
    const disconnected =
      err instanceof TransportError
        ? err
        : new TransportError("disconnected", "daemon connection closed", { cause: err });
    for (const [id, pending] of this.#pending) {
      this.#pending.delete(id);
      pending.reject(
        new TransportError("unknown_outcome", "connection lost before RPC response", {
          cause: disconnected,
        }),
      );
    }
    for (const stream of this.#streams.values()) {
      stream.queue.fail(
        new TransportError("disconnected", "event stream disconnected", {
          resumeCursor: stream.cursor,
          cause: disconnected,
        }),
      );
    }
  }

  #dispatch(line: string): void {
    let frame: InboundFrame;
    try {
      if (Buffer.byteLength(line, "utf8") > MAX_RPC_FRAME_BYTES) {
        throw new TransportError("oversized", "RPC frame exceeds byte bound");
      }
      const parsed = parseJson(line, { maxBytes: MAX_RPC_FRAME_BYTES });
      frame = decodeInboundFrame(parsed, Buffer.byteLength(line, "utf8"));
    } catch (err) {
      this.#failClosed(normalizeDecodeFailure(err));
      return;
    }

    if (frame.kind === "hello_ok") {
      const pending = this.#pending.get(frame.id);
      if (!pending) {
        this.#failClosed(new TransportError("protocol", "hello_ok for unknown request id"));
        return;
      }
      try {
        assertHelloCompatible(frame);
      } catch (err) {
        pending.reject(err);
        return;
      }
      pending.resolve(null);
      return;
    }

    if (frame.kind === "response") {
      const pending = this.#pending.get(frame.id);
      if (!pending) {
        return;
      }
      if (frame.errorRaw !== undefined) {
        try {
          pending.reject(new RpcError(decodeApiError(frame.errorRaw)));
        } catch (err) {
          pending.reject(normalizeDecodeFailure(err));
        }
        return;
      }
      if (frame.result === undefined) {
        pending.reject(new TransportError("protocol", "RPC response missing result"));
        return;
      }
      pending.resolve(frame.result);
      return;
    }

    if (frame.kind === "event") {
      const stream = this.#streams.get(frame.id);
      if (!stream) {
        return;
      }
      try {
        const event = decodeEvent(frame.eventRaw, { maxBytes: MAX_EVENT_BYTES });
        const cursor = frame.cursor === undefined ? event.seq : frame.cursor;
        if (cursor !== event.seq) {
          throw new TransportError("protocol", "event cursor does not match seq");
        }
        if (!stream.queue.push(event)) {
          this.#sendCancel(frame.id);
          stream.queue.fail(
            new TransportError("lagged", "event stream buffer exceeded", {
              resumeCursor: stream.cursor,
            }),
          );
        }
      } catch (err) {
        stream.queue.fail(normalizeDecodeFailure(err));
      }
      return;
    }

    const stream = this.#streams.get(frame.id);
    if (!stream) {
      return;
    }
    if (frame.reason === "complete") {
      stream.queue.close();
      return;
    }
    if (frame.reason === "lagged") {
      stream.queue.fail(
        new TransportError("lagged", "event subscription lagged", {
          resumeCursor: frame.cursor,
        }),
      );
      return;
    }
    if (frame.errorRaw !== undefined) {
      try {
        stream.queue.fail(new RpcError(decodeApiError(frame.errorRaw)));
      } catch (err) {
        stream.queue.fail(normalizeDecodeFailure(err));
      }
      return;
    }
    stream.queue.fail(
      new TransportError(
        frame.reason === "cancelled" ? "cancelled" : "disconnected",
        "event subscription ended",
        { resumeCursor: frame.cursor },
      ),
    );
  }

  #send(value: unknown): void {
    const channel = this.#channel;
    if (!channel) {
      throw new TransportError("disconnected", "not connected to daemon");
    }
    const frame = encodeFrame(value);
    channel.send(frame);
  }

  #sendCancel(id: string): void {
    if (!this.#channel || this.#closed) {
      return;
    }
    try {
      this.#send({
        schema: RPC_SCHEMA,
        schema_version: RPC_SCHEMA_VERSION,
        kind: "cancel",
        id,
      });
    } catch {
      // Peer already gone; local abort still settles the waiter.
    }
  }

  #failPending(err: TransportError): void {
    for (const [id, pending] of this.#pending) {
      this.#pending.delete(id);
      pending.reject(err);
    }
    for (const [id, stream] of this.#streams) {
      this.#streams.delete(id);
      stream.queue.fail(err);
    }
  }

  #failClosed(err: TransportError): void {
    this.#connected = false;
    const channel = this.#channel;
    this.#channel = null;
    this.#failPending(err);
    channel?.close();
  }

  #assertOpen(): void {
    if (this.#closed) {
      throw new TransportError("cancelled", "transport closed");
    }
  }
}

export function createMemoryChannelPair(): {
  client: TransportChannel;
  server: TransportChannel;
} {
  const toClient = new FrameQueue();
  const toServer = new FrameQueue();
  const client: TransportChannel = {
    send(frame: string): void {
      toServer.push(frame);
    },
    frames(): AsyncIterable<string> {
      return toClient;
    },
    close(): void {
      toClient.close();
      toServer.close();
    },
  };
  const server: TransportChannel = {
    send(frame: string): void {
      toClient.push(frame);
    },
    frames(): AsyncIterable<string> {
      return toServer;
    },
    close(): void {
      toClient.close();
      toServer.close();
    },
  };
  return { client, server };
}

function resolveChannelFactory(options: LocalConnectOptions): ChannelFactory {
  const specified = [
    options.socketPath !== undefined,
    options.websocketUrl !== undefined,
    options.channelFactory !== undefined,
  ].filter(Boolean).length;
  if (specified !== 1) {
    throw new TransportError(
      "protocol",
      "exactly one of socketPath, websocketUrl, or channelFactory is required",
    );
  }
  if (options.channelFactory) {
    return options.channelFactory;
  }
  if (options.socketPath !== undefined) {
    const path = assertLocalIpcPath(options.socketPath);
    return () => connectUnixOrPipe(path);
  }
  const url = assertLoopbackWebSocketUrl(options.websocketUrl as string);
  return () => connectLoopbackWebSocket(url);
}

function requireAuthToken(token: string | undefined): string {
  if (token === undefined || token.length === 0) {
    throw new TransportError("auth.required", "local daemon auth token is required");
  }
  if (token.includes("\n") || token.includes("\r") || token.includes("\0")) {
    throw new TransportError("auth.required", "local daemon auth token is invalid");
  }
  if (Buffer.byteLength(token, "utf8") > MAX_AUTH_HANDLE_BYTES) {
    throw new TransportError("oversized", "local daemon auth token exceeds byte bound");
  }
  return token;
}

function assertLocalIpcPath(path: string): string {
  if (path.includes("\0") || path.includes("\n")) {
    throw new TransportError("protocol", "invalid local IPC path");
  }
  if (path.includes("://") || /[a-z][a-z0-9+.-]*:/i.test(path.split("\\")[0] ?? path)) {
    throw new TransportError("protocol", "IPC path must not be a URL");
  }
  if (path.startsWith("\\\\.\\pipe\\") && path.length > "\\\\.\\pipe\\".length) {
    return path;
  }
  if (!path.startsWith("/") || path.length < 2) {
    throw new TransportError("protocol", "IPC path must be an absolute local path");
  }
  return path;
}

function assertLoopbackWebSocketUrl(raw: string): URL {
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    throw new TransportError("protocol", "invalid websocket URL");
  }
  if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw new TransportError("protocol", "websocket URL must use ws: or wss:");
  }
  if (url.username !== "" || url.password !== "") {
    throw new TransportError("protocol", "websocket URL must not contain userinfo");
  }
  const host = url.hostname.toLowerCase();
  if (host !== "127.0.0.1" && host !== "[::1]" && host !== "::1") {
    throw new TransportError("protocol", "websocket URL must target loopback");
  }
  return url;
}

function connectUnixOrPipe(path: string): Promise<TransportChannel> {
  return new Promise((resolve, reject) => {
    const socket = net.connect({ path });
    const onError = (err: Error): void => {
      cleanup();
      reject(new TransportError("disconnected", "local IPC connect failed", { cause: err }));
    };
    const cleanup = (): void => {
      socket.off("connect", onConnect);
      socket.off("error", onError);
    };
    const onConnect = (): void => {
      cleanup();
      resolve(socketChannel(socket));
    };
    socket.once("connect", onConnect);
    socket.once("error", onError);
  });
}

function connectLoopbackWebSocket(url: URL): Promise<TransportChannel> {
  const WS = (globalThis as { WebSocket?: new (url: string) => MinimalWebSocket }).WebSocket;
  if (!WS) {
    return Promise.reject(new TransportError("protocol", "WebSocket is not available"));
  }
  return new Promise((resolve, reject) => {
    const ws = new WS(url.toString());
    const onError = (): void => {
      cleanup();
      reject(new TransportError("disconnected", "loopback websocket connect failed"));
    };
    const cleanup = (): void => {
      ws.removeEventListener("open", onOpen);
      ws.removeEventListener("error", onError);
    };
    const onOpen = (): void => {
      cleanup();
      resolve(webSocketChannel(ws));
    };
    ws.addEventListener("open", onOpen);
    ws.addEventListener("error", onError);
  });
}

type MinimalWebSocket = {
  send(data: string): void;
  close(): void;
  addEventListener(type: string, listener: () => void): void;
  removeEventListener(type: string, listener: () => void): void;
};

function socketChannel(socket: net.Socket): TransportChannel {
  const queue = new FrameQueue();
  let buf = Buffer.alloc(0);
  const onData = (chunk: Buffer): void => {
    buf = Buffer.concat([buf, chunk]);
    if (buf.length > MAX_RPC_FRAME_BYTES && buf.indexOf(0x0a) < 0) {
      queue.fail(new TransportError("oversized", "RPC frame exceeds byte bound"));
      socket.destroy();
      return;
    }
    for (;;) {
      const idx = buf.indexOf(0x0a);
      if (idx < 0) {
        break;
      }
      const raw = buf.subarray(0, idx);
      buf = buf.subarray(idx + 1);
      const line =
        raw.length > 0 && raw[raw.length - 1] === 0x0d ? raw.subarray(0, raw.length - 1) : raw;
      if (line.length > MAX_RPC_FRAME_BYTES) {
        queue.fail(new TransportError("oversized", "RPC frame exceeds byte bound"));
        socket.destroy();
        return;
      }
      if (line.length === 0) {
        continue;
      }
      queue.push(line.toString("utf8"));
    }
  };
  socket.on("data", onData);
  socket.on("error", (err) => {
    queue.fail(new TransportError("disconnected", "local IPC error", { cause: err }));
  });
  socket.on("close", () => {
    queue.close();
  });
  return {
    send(frame: string): void {
      const payload = `${frame}\n`;
      const size = Buffer.byteLength(payload, "utf8");
      if (socket.writableLength + size > MAX_WRITE_BUFFER_BYTES) {
        queue.fail(new TransportError("lagged", "IPC write buffer exceeded"));
        socket.destroy();
        return;
      }
      socket.write(payload);
    },
    frames(): AsyncIterable<string> {
      return queue;
    },
    close(): void {
      socket.removeListener("data", onData);
      socket.destroy();
      queue.close();
    },
  };
}

function webSocketChannel(ws: MinimalWebSocket): TransportChannel {
  const queue = new FrameQueue();
  const onMessage = (event: { data?: unknown } | undefined): void => {
    const data = event?.data;
    if (typeof data !== "string") {
      queue.fail(new TransportError("protocol", "websocket frame must be UTF-8 text"));
      ws.close();
      return;
    }
    if (Buffer.byteLength(data, "utf8") > MAX_RPC_FRAME_BYTES) {
      queue.fail(new TransportError("oversized", "RPC frame exceeds byte bound"));
      ws.close();
      return;
    }
    queue.push(data);
  };
  const onClose = (): void => {
    queue.close();
  };
  const onError = (): void => {
    queue.fail(new TransportError("disconnected", "loopback websocket error"));
  };
  ws.addEventListener("message", onMessage as () => void);
  ws.addEventListener("close", onClose);
  ws.addEventListener("error", onError);
  return {
    send(frame: string): void {
      ws.send(frame);
    },
    frames(): AsyncIterable<string> {
      return queue;
    },
    close(): void {
      ws.removeEventListener("message", onMessage as () => void);
      ws.removeEventListener("close", onClose);
      ws.removeEventListener("error", onError);
      ws.close();
      queue.close();
    },
  };
}

function encodeFrame(value: unknown): string {
  let text: string;
  try {
    text = JSON.stringify(value) as string;
  } catch {
    throw new TransportError("protocol", "RPC frame is not JSON-serializable");
  }
  if (typeof text !== "string" || text.includes("\n") || text.includes("\r")) {
    throw new TransportError("protocol", "RPC frame must be single-line JSON");
  }
  if (Buffer.byteLength(text, "utf8") > MAX_CONTROL_FRAME_BYTES) {
    throw new TransportError("oversized", "RPC frame exceeds byte bound");
  }
  return text;
}

function decodeInboundFrame(input: unknown, lineBytes: number): InboundFrame {
  const obj = asRecord(input, "");
  const schema = asString(obj.schema, "schema");
  if (schema !== RPC_SCHEMA) {
    throw new TransportError("unsupported_schema", "unsupported RPC schema");
  }
  const schemaVersion = asUint(obj.schema_version, "schema_version");
  if (schemaVersion !== RPC_SCHEMA_VERSION) {
    throw new TransportError("unsupported_schema", "unsupported RPC schema version");
  }
  const kind = asString(obj.kind, "kind");
  const id = asUuid(obj.id, "id");
  if (kind !== "event" && lineBytes > MAX_CONTROL_FRAME_BYTES) {
    throw new TransportError("oversized", "RPC control frame exceeds byte bound");
  }
  if (kind === "hello" || kind === "request" || kind === "cancel") {
    throw new TransportError("protocol", "unexpected client RPC kind from daemon");
  }
  if (kind === "hello_ok") {
    rejectUnknownFields(obj, HELLO_OK_FIELDS, "");
    const sha = obj.wire_schema_sha256;
    return {
      kind: "hello_ok",
      id,
      wire_schema: asString(obj.wire_schema, "wire_schema"),
      wire_schema_version: asUint(obj.wire_schema_version, "wire_schema_version"),
      wire_schema_sha256: sha === undefined || sha === null ? null : asString(sha, "wire_schema_sha256"),
    };
  }
  if (kind === "response") {
    rejectUnknownFields(obj, RESPONSE_FIELDS, "");
    const hasResult = Object.prototype.hasOwnProperty.call(obj, "result");
    const hasError = Object.prototype.hasOwnProperty.call(obj, "error");
    if (hasResult === hasError) {
      throw new TransportError("protocol", "RPC response must contain exactly one of result or error");
    }
    if (hasError) {
      return { kind: "response", id, result: undefined, errorRaw: obj.error };
    }
    return { kind: "response", id, result: asJsonValue(obj.result), errorRaw: undefined };
  }
  if (kind === "event") {
    rejectUnknownFields(obj, EVENT_FIELDS, "");
    const encoded = JSON.stringify(obj.event);
    if (encoded === undefined || Buffer.byteLength(encoded, "utf8") > MAX_EVENT_BYTES) {
      throw new TransportError("oversized", "event payload exceeds byte bound");
    }
    const cursor = obj.cursor === undefined ? undefined : asUint(obj.cursor, "cursor");
    return { kind: "event", id, cursor, eventRaw: obj.event };
  }
  if (kind === "stream_end") {
    rejectUnknownFields(obj, STREAM_END_FIELDS, "");
    const reason = asString(obj.reason, "reason");
    if (!(STREAM_END_REASONS as readonly string[]).includes(reason)) {
      throw new TransportError("protocol", "unknown stream_end reason");
    }
    return {
      kind: "stream_end",
      id,
      cursor: asUint(obj.cursor, "cursor"),
      reason: reason as StreamEndReason,
      errorRaw: obj.error === undefined || obj.error === null ? undefined : obj.error,
    };
  }
  throw new TransportError("protocol", "unknown RPC frame kind");
}

function assertHelloCompatible(frame: HelloOk): void {
  if (frame.wire_schema !== WIRE_CATALOG_SCHEMA) {
    throw new TransportError("unsupported_schema", "daemon wire schema mismatch");
  }
  if (frame.wire_schema_version !== WIRE_SCHEMA_VERSION) {
    throw new TransportError("unsupported_schema", "daemon wire schema version mismatch");
  }
  if (frame.wire_schema_sha256 !== null && frame.wire_schema_sha256 !== WIRE_SCHEMA_SHA256) {
    throw new TransportError("unsupported_schema", "daemon wire schema hash mismatch");
  }
}

function normalizeDecodeFailure(err: unknown): TransportError {
  if (err instanceof TransportError) {
    return err;
  }
  if (err instanceof RpcError) {
    return new TransportError("protocol", err.message, { apiError: err.error, cause: err });
  }
  if (err instanceof DecodeError) {
    const code: TransportErrorCode =
      err.code === "cancelled"
        ? "cancelled"
        : err.code === "oversized"
          ? "oversized"
          : err.code === "unsupported_schema"
            ? "unsupported_schema"
            : "protocol";
    return new TransportError(code, "RPC frame decode failed", { cause: err });
  }
  return new TransportError("protocol", "RPC frame decode failed", { cause: err });
}

function isReconnectable(err: unknown): boolean {
  if (err instanceof TransportError) {
    return err.code === "disconnected" || err.code === "lagged";
  }
  return false;
}

function abortToError(signal: AbortSignal): TransportError {
  const reason = signal.reason;
  if (reason instanceof TransportError) {
    return reason;
  }
  if (reason instanceof DecodeError && reason.code === "cancelled") {
    return new TransportError("cancelled", "request cancelled", { cause: reason });
  }
  if (reason && typeof reason === "object" && "name" in reason && reason.name === "TimeoutError") {
    return new TransportError("timeout", "RPC request timed out", { cause: reason });
  }
  return new TransportError("cancelled", "request cancelled", { cause: reason });
}

function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted) {
    throw abortToError(signal);
  }
}

function normalizeReconnect(input: Partial<ReconnectPolicy> | undefined): ReconnectPolicy {
  return {
    maxAttempts: requirePositiveInt(
      input?.maxAttempts ?? DEFAULT_RECONNECT_ATTEMPTS,
      "reconnect.maxAttempts",
    ),
    backoffMs: requireNonNegativeInt(
      input?.backoffMs ?? DEFAULT_RECONNECT_BACKOFF_MS,
      "reconnect.backoffMs",
    ),
  };
}

function requirePositiveInt(value: number, name: string): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value <= 0) {
    throw new TransportError("protocol", `${name} must be a positive integer`);
  }
  return value;
}

function requireNonNegativeInt(value: number, name: string): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new TransportError("protocol", `${name} must be a non-negative integer`);
  }
  return value;
}

function newRequestId(): string {
  return crypto.randomUUID();
}

function asRecord(value: unknown, path: string): { [key: string]: unknown } {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new TransportError("protocol", `${path || "frame"} must be an object`);
  }
  const out: { [key: string]: unknown } = Object.create(null) as { [key: string]: unknown };
  for (const key of Object.keys(value as { [key: string]: unknown })) {
    if (PROTOTYPE_KEYS.has(key)) {
      throw new TransportError("protocol", "prototype-polluting RPC key");
    }
    out[key] = (value as { [key: string]: unknown })[key];
  }
  return out;
}

function rejectUnknownFields(
  obj: { [key: string]: unknown },
  allowed: readonly string[],
  path: string,
): void {
  for (const key of Object.keys(obj)) {
    if (!allowed.includes(key)) {
      throw new TransportError("protocol", `unknown field ${path === "" ? key : `${path}.${key}`}`);
    }
  }
}

function asString(value: unknown, path: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new TransportError("protocol", `${path} must be a non-empty string`);
  }
  return value;
}

function asUuid(value: unknown, path: string): string {
  const text = asString(value, path);
  if (!UUID_PATTERN.test(text)) {
    throw new TransportError("protocol", `${path} must be a lowercase UUID`);
  }
  return text;
}

function asUint(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new TransportError("protocol", `${path} must be an unsigned integer`);
  }
  if (value > Number.MAX_SAFE_INTEGER) {
    throw new TransportError("oversized", `${path} exceeds safe integer bound`);
  }
  return value;
}

function asJsonValue(value: unknown): JsonValue {
  if (value === undefined) {
    throw new TransportError("protocol", "JSON value is missing");
  }
  return value as JsonValue;
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(abortToError(signal));
      return;
    }
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = (): void => {
      clearTimeout(timer);
      reject(abortToError(signal));
    };
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

class FrameQueue implements AsyncIterable<string> {
  readonly #items: string[] = [];
  readonly #waiters: Array<(result: IteratorResult<string>) => void> = [];
  #done: TransportError | "closed" | null = null;

  push(frame: string): void {
    if (this.#done !== null) {
      return;
    }
    const waiter = this.#waiters.shift();
    if (waiter) {
      waiter({ value: frame, done: false });
      return;
    }
    this.#items.push(frame);
  }

  fail(err: TransportError): void {
    if (this.#done !== null) {
      return;
    }
    this.#done = err;
    this.#flushWaiters();
  }

  close(): void {
    if (this.#done !== null) {
      return;
    }
    this.#done = "closed";
    this.#flushWaiters();
  }

  async *[Symbol.asyncIterator](): AsyncGenerator<string, void, void> {
    for (;;) {
      const item = this.#items.shift();
      if (item !== undefined) {
        yield item;
        continue;
      }
      if (this.#done instanceof TransportError) {
        throw this.#done;
      }
      if (this.#done === "closed") {
        return;
      }
      const next = await new Promise<IteratorResult<string>>((resolve) => {
        this.#waiters.push(resolve);
      });
      if (next.done) {
        if (this.#done instanceof TransportError) {
          throw this.#done;
        }
        return;
      }
      yield next.value;
    }
  }

  #flushWaiters(): void {
    const result: IteratorResult<string> =
      this.#done instanceof TransportError
        ? { value: undefined, done: true }
        : { value: undefined, done: true };
    while (this.#waiters.length > 0) {
      this.#waiters.shift()?.(result);
    }
    if (this.#done instanceof TransportError) {
      // Iterator observes the error on next pull.
    }
  }
}

class BoundedQueue<T> {
  readonly #max: number;
  readonly #items: T[] = [];
  readonly #waiters: Array<(result: IteratorResult<T>) => void> = [];
  #done: unknown | "closed" | null = null;

  constructor(max: number) {
    this.#max = max;
  }

  push(item: T): boolean {
    if (this.#done !== null) {
      return false;
    }
    const waiter = this.#waiters.shift();
    if (waiter) {
      waiter({ value: item, done: false });
      return true;
    }
    if (this.#items.length >= this.#max) {
      return false;
    }
    this.#items.push(item);
    return true;
  }

  fail(err: unknown): void {
    if (this.#done !== null) {
      return;
    }
    this.#done = err;
    this.#wake(true);
  }

  close(): void {
    if (this.#done !== null) {
      return;
    }
    this.#done = "closed";
    this.#wake(true);
  }

  async next(): Promise<IteratorResult<T>> {
    const item = this.#items.shift();
    if (item !== undefined) {
      return { value: item, done: false };
    }
    if (this.#done === "closed") {
      return { value: undefined as T, done: true };
    }
    if (this.#done !== null) {
      throw this.#done;
    }
    return new Promise<IteratorResult<T>>((resolve, reject) => {
      this.#waiters.push((result) => {
        if (result.done && this.#done !== null && this.#done !== "closed") {
          reject(this.#done);
          return;
        }
        resolve(result);
      });
    });
  }

  #wake(done: boolean): void {
    while (this.#waiters.length > 0) {
      this.#waiters.shift()?.({ value: undefined as T, done });
    }
  }
}

export function isTransportErrorCode(code: string): code is TransportErrorCode {
  return (TRANSPORT_CODES as readonly string[]).includes(code);
}
