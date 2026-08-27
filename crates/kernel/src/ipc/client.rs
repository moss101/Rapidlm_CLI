//! Reconnectable local daemon IPC client.
//!
//! Transient Unix-socket loss reconnects. Event subscriptions resume from the
//! last committed cursor and skip already-processed seqs. Request IDs are
//! never reused. Non-idempotent RPCs that lose their response report
//! [`DaemonClientError::UnknownOutcome`] instead of replaying.

use std::error::Error;
use std::fmt;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use event_ledger::event::ErasedEventEnvelope;
use protocol::{ApiError, SessionId, TurnId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::server::{
    IPC_SCHEMA, IpcError, IpcLimits, ListenSpec, MAX_FRAME_BYTES, MAX_PIPE_NAME_BYTES,
    MAX_REQUEST_ID_BYTES, MAX_SOCKET_PATH_BYTES, SOCKET_DIR_MODE, read_frame, write_frame,
};
use crate::session::fork::ForkSession;
use crate::session::projection::SessionSnapshot;
use crate::session::service::CreateSession;
use crate::{
    CancellationToken, Interrupt, ResolveApproval, RewindSession, SubmitTurn, SubscribeEvents,
};

/// Ceiling on reconnect attempts for one unary retry loop or one stream wait.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 8;

/// Maximum time spent waiting for a socket that is not yet present.
pub const MAX_CONNECT_WAIT: Duration = Duration::from_secs(5);

const RECONNECT_BACKOFF: Duration = Duration::from_millis(20);
const STREAM_POLL: Duration = Duration::from_millis(25);

/// Client that talks KernelClient methods over local IPC.
#[derive(Clone)]
pub struct DaemonClient {
    spec: ListenSpec,
    cancel: CancellationToken,
    limits: IpcLimits,
    shared: Arc<Mutex<Shared>>,
}

struct Shared {
    next_request: u64,
    #[cfg(unix)]
    unary: Option<std::os::unix::net::UnixStream>,
}

/// Resume-capable event subscription. Reconnects from [`Self::cursor`].
pub struct DaemonEventStream {
    client: DaemonClient,
    session_id: SessionId,
    cursor: u64,
    #[cfg(unix)]
    conn: Option<StreamConn>,
    closed: bool,
    reconnects: u32,
}

#[cfg(unix)]
struct StreamConn {
    stream: std::os::unix::net::UnixStream,
    request_id: String,
}

/// Typed client failure. Display never includes frame bytes or payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonClientError {
    Cancelled {
        resume_cursor: u64,
    },
    Transport(IpcError),
    Api(ApiError),
    /// Non-idempotent RPC was sent (or may have been sent) with no response.
    UnknownOutcome {
        method: &'static str,
        request_id: String,
    },
    Decode,
}

/// Whether a wire method may be retried after transport loss.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RequestIdempotency {
    Idempotent,
    NonIdempotent,
}

/// Turn handle decoded from the daemon wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
pub struct IpcTurnHandle {
    session_id: SessionId,
    turn_id: TurnId,
    seq: u64,
}

/// Rewind result decoded from the daemon wire.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct IpcRewindResult {
    snapshot: SessionSnapshot,
    through_seq: u64,
    current_seq: u64,
}

#[derive(Serialize)]
struct WireRequest<'a> {
    schema: u16,
    id: &'a str,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
struct WireInbound {
    schema: u16,
    id: String,
    #[serde(default)]
    ok: Option<Value>,
    #[serde(default)]
    error: Option<ApiError>,
    #[serde(default)]
    event: Option<ErasedEventEnvelope>,
    #[serde(default)]
    cursor: Option<u64>,
    #[serde(default)]
    stream_end: Option<StreamEndBody>,
}

#[derive(Deserialize)]
struct StreamEndBody {
    cursor: u64,
}

#[derive(Deserialize)]
struct SubscribedOk {
    #[serde(default)]
    subscribed: bool,
    #[serde(default)]
    #[allow(dead_code)]
    cursor: u64,
}

enum Inbound {
    Ok {
        id: String,
        value: Value,
    },
    Err {
        id: String,
        error: ApiError,
    },
    Event {
        id: String,
        event: ErasedEventEnvelope,
        cursor: u64,
    },
    StreamEnd {
        id: String,
        cursor: u64,
    },
}

impl DaemonClient {
    /// Validate the endpoint and establish a unary connection.
    pub fn connect(spec: ListenSpec, cancel: CancellationToken) -> Result<Self, DaemonClientError> {
        Self::connect_with_limits(spec, cancel, IpcLimits::default())
    }

    pub fn connect_with_limits(
        spec: ListenSpec,
        cancel: CancellationToken,
        limits: IpcLimits,
    ) -> Result<Self, DaemonClientError> {
        if cancel.is_cancelled() {
            return Err(DaemonClientError::cancelled(0));
        }
        validate_connect_spec(&spec)?;
        validate_limits(limits)?;
        let client = Self {
            spec,
            cancel,
            limits,
            shared: Arc::new(Mutex::new(Shared {
                next_request: 0,
                #[cfg(unix)]
                unary: None,
            })),
        };
        let stream = client.dial()?;
        client.store_unary(stream);
        Ok(client)
    }

    pub fn endpoint(&self) -> &ListenSpec {
        &self.spec
    }

    pub fn limits(&self) -> IpcLimits {
        self.limits
    }

    pub fn create_session(&self, req: CreateSession) -> Result<SessionSnapshot, DaemonClientError> {
        let params = serde_json::json!({
            "project_id": req.project_id(),
            "actor": req.actor(),
            "trace_id": req.trace_id(),
        });
        self.rpc_non_idempotent("create_session", params)
    }

    pub fn get_session(&self, id: SessionId) -> Result<SessionSnapshot, DaemonClientError> {
        let params = serde_json::json!({ "session_id": id });
        self.rpc_idempotent("get_session", params)
    }

    pub fn submit_turn(&self, req: SubmitTurn) -> Result<IpcTurnHandle, DaemonClientError> {
        let params = serde_json::json!({
            "session_id": req.session_id(),
            "expected_seq": req.expected_seq(),
            "actor": req.actor(),
            "trace_id": req.trace_id(),
        });
        self.rpc_non_idempotent("submit_turn", params)
    }

    pub fn interrupt(&self, req: Interrupt) -> Result<(), DaemonClientError> {
        let params = serde_json::json!({
            "session_id": req.session_id(),
            "reason": req.reason().as_str(),
            "actor": req.actor(),
            "trace_id": req.trace_id(),
        });
        let _: Value = self.rpc_idempotent("interrupt", params)?;
        Ok(())
    }

    pub fn subscribe(&self, req: SubscribeEvents) -> Result<DaemonEventStream, DaemonClientError> {
        let mut stream = DaemonEventStream {
            client: self.clone(),
            session_id: req.session_id(),
            cursor: req.from_seq(),
            #[cfg(unix)]
            conn: None,
            closed: false,
            reconnects: 0,
        };
        stream.ensure_subscribed()?;
        Ok(stream)
    }

    pub fn approve(&self, req: ResolveApproval) -> Result<(), DaemonClientError> {
        let params = serde_json::json!({
            "session_id": req.session_id(),
            "expected_seq": req.expected_seq(),
            "decision": req.decision().as_str(),
            "actor": req.actor(),
            "trace_id": req.trace_id(),
        });
        let _: Value = self.rpc_non_idempotent("approve", params)?;
        Ok(())
    }

    pub fn fork_session(&self, req: ForkSession) -> Result<SessionSnapshot, DaemonClientError> {
        let params = serde_json::json!({
            "source": req.source(),
            "at_seq": req.at_seq(),
            "actor": req.actor(),
            "trace_id": req.trace_id(),
        });
        self.rpc_non_idempotent("fork_session", params)
    }

    pub fn rewind(&self, req: RewindSession) -> Result<IpcRewindResult, DaemonClientError> {
        let params = serde_json::json!({
            "session_id": req.session_id(),
            "to_seq": req.to_seq(),
        });
        self.rpc_idempotent("rewind", params)
    }

    fn rpc_idempotent<T: for<'de> Deserialize<'de>>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<T, DaemonClientError> {
        let mut last = DaemonClientError::Transport(IpcError::Io);
        for _ in 0..MAX_RECONNECT_ATTEMPTS {
            self.check_cancel(0)?;
            match self.exchange(method, RequestIdempotency::Idempotent, params.clone()) {
                Ok(value) => return decode_ok(value),
                Err(err) if err.is_retryable_transport() => {
                    last = err;
                    self.drop_unary();
                    if !self.sleep_backoff() {
                        return Err(DaemonClientError::cancelled(0));
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Err(last)
    }

    fn rpc_non_idempotent<T: for<'de> Deserialize<'de>>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<T, DaemonClientError> {
        self.check_cancel(0)?;
        decode_ok(self.exchange(method, RequestIdempotency::NonIdempotent, params)?)
    }

    fn exchange(
        &self,
        method: &'static str,
        class: RequestIdempotency,
        params: Value,
    ) -> Result<Value, DaemonClientError> {
        let mut stream = self.take_unary()?;
        let request_id = self.next_request_id()?;
        let body = encode_request(&request_id, method, params)?;
        match write_frame(&mut stream, &body, self.limits.max_frame_bytes()) {
            Ok(()) => {}
            Err(err) if class == RequestIdempotency::NonIdempotent && write_may_have_sent(&err) => {
                return Err(DaemonClientError::UnknownOutcome { method, request_id });
            }
            Err(err) => return Err(DaemonClientError::Transport(err)),
        }
        let reply = match read_frame(&mut stream, self.limits.max_frame_bytes()) {
            Ok(bytes) => bytes,
            Err(_) if class == RequestIdempotency::NonIdempotent => {
                return Err(DaemonClientError::UnknownOutcome { method, request_id });
            }
            Err(err) => return Err(DaemonClientError::Transport(err)),
        };
        let inbound = decode_inbound(&reply)?;
        match inbound {
            Inbound::Ok { id, value } => {
                ensure_id(&id, &request_id)?;
                self.store_unary(stream);
                Ok(value)
            }
            Inbound::Err { id, error } => {
                ensure_id(&id, &request_id)?;
                self.store_unary(stream);
                Err(DaemonClientError::Api(error))
            }
            Inbound::Event { .. } | Inbound::StreamEnd { .. } => Err(DaemonClientError::Decode),
        }
    }

    fn take_unary(&self) -> Result<UnaryStream, DaemonClientError> {
        self.check_cancel(0)?;
        #[cfg(unix)]
        {
            if let Some(existing) = self.lock().unary.take() {
                return Ok(existing);
            }
        }
        self.dial()
    }

    fn store_unary(&self, stream: UnaryStream) {
        #[cfg(unix)]
        {
            let mut shared = self.lock();
            if shared.unary.is_none() {
                shared.unary = Some(stream);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = stream;
        }
    }

    fn drop_unary(&self) {
        #[cfg(unix)]
        {
            self.lock().unary = None;
        }
    }

    fn next_request_id(&self) -> Result<String, DaemonClientError> {
        let mut shared = self.lock();
        let n = shared.next_request;
        shared.next_request = n
            .checked_add(1)
            .ok_or(DaemonClientError::Transport(IpcError::Internal))?;
        let id = format!("r{n:016x}");
        if id.is_empty() || id.len() > MAX_REQUEST_ID_BYTES {
            return Err(DaemonClientError::Transport(IpcError::Internal));
        }
        Ok(id)
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check_cancel(&self, resume_cursor: u64) -> Result<(), DaemonClientError> {
        if self.cancel.is_cancelled() {
            Err(DaemonClientError::cancelled(resume_cursor))
        } else {
            Ok(())
        }
    }

    fn sleep_backoff(&self) -> bool {
        if self.cancel.is_cancelled() {
            return false;
        }
        std::thread::sleep(RECONNECT_BACKOFF);
        !self.cancel.is_cancelled()
    }

    fn dial(&self) -> Result<UnaryStream, DaemonClientError> {
        self.check_cancel(0)?;
        validate_connect_spec(&self.spec)?;
        #[cfg(unix)]
        {
            match &self.spec {
                ListenSpec::UnixSocket { path } => self.dial_unix(path),
                ListenSpec::NamedPipe { .. } => {
                    Err(DaemonClientError::Transport(IpcError::UnsupportedEndpoint))
                }
                ListenSpec::TcpLoopback { .. } | ListenSpec::WebSocketLoopback { .. } => Err(
                    DaemonClientError::Transport(IpcError::NetworkTransportDisabled),
                ),
            }
        }
        #[cfg(not(unix))]
        {
            match &self.spec {
                ListenSpec::TcpLoopback { .. } | ListenSpec::WebSocketLoopback { .. } => Err(
                    DaemonClientError::Transport(IpcError::NetworkTransportDisabled),
                ),
                _ => Err(DaemonClientError::Transport(IpcError::UnsupportedEndpoint)),
            }
        }
    }

    #[cfg(unix)]
    fn dial_unix(&self, path: &Path) -> Result<std::os::unix::net::UnixStream, DaemonClientError> {
        let deadline = Instant::now() + connect_wait(self.limits);
        loop {
            self.check_cancel(0)?;
            match try_connect_unix(path, self.limits) {
                Ok(stream) => return Ok(stream),
                Err(err) if retryable_connect(err) && Instant::now() < deadline => {
                    if !self.sleep_backoff() {
                        return Err(DaemonClientError::cancelled(0));
                    }
                }
                Err(err) => return Err(DaemonClientError::Transport(err)),
            }
        }
    }
}

impl DaemonEventStream {
    /// Last seq delivered to this consumer (`from_seq` if nothing received).
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Wait for the next committed event after [`Self::cursor`].
    pub fn recv(&mut self) -> Result<ErasedEventEnvelope, DaemonClientError> {
        loop {
            self.client.check_cancel(self.cursor)?;
            if self.closed {
                return Err(DaemonClientError::cancelled(self.cursor));
            }
            self.ensure_subscribed()?;
            match self.read_stream_frame() {
                Ok(Inbound::Event { id, event, cursor }) => {
                    if !self.event_id_matches(&id) {
                        self.drop_conn();
                        self.note_disconnect()?;
                        continue;
                    }
                    let seq = event.seq().max(cursor);
                    if seq <= self.cursor {
                        continue;
                    }
                    self.cursor = seq;
                    self.reconnects = 0;
                    return Ok(event);
                }
                Ok(Inbound::StreamEnd { id, cursor }) => {
                    if !self.event_id_matches(&id) {
                        self.drop_conn();
                        self.note_disconnect()?;
                        continue;
                    }
                    if cursor > self.cursor {
                        self.cursor = cursor;
                    }
                    self.drop_conn();
                    if self.client.cancel.is_cancelled() || self.closed {
                        return Err(DaemonClientError::cancelled(self.cursor));
                    }
                    self.note_disconnect()?;
                }
                Ok(Inbound::Ok { .. } | Inbound::Err { .. }) => {
                    self.drop_conn();
                    return Err(DaemonClientError::Decode);
                }
                Err(err) if err.is_retryable_transport() => {
                    self.drop_conn();
                    self.note_disconnect()?;
                }
                Err(err) => {
                    self.drop_conn();
                    return Err(err);
                }
            }
        }
    }

    /// Non-blocking poll. `Ok(None)` means no new committed event is queued.
    pub fn try_recv(&mut self) -> Result<Option<ErasedEventEnvelope>, DaemonClientError> {
        self.client.check_cancel(self.cursor)?;
        if self.closed {
            return Err(DaemonClientError::cancelled(self.cursor));
        }
        if self.conn_is_none() {
            match self.ensure_subscribed() {
                Ok(()) => {}
                Err(err) if err.is_retryable_transport() => return Ok(None),
                Err(err) => return Err(err),
            }
        }
        #[cfg(unix)]
        {
            if let Some(conn) = self.conn.as_mut()
                && set_timeouts(&mut conn.stream, Some(STREAM_POLL), self.client.limits).is_err() {
                    self.drop_conn();
                    return Ok(None);
                }
        }
        match self.read_stream_frame() {
            Ok(Inbound::Event { id, event, cursor }) => {
                if !self.event_id_matches(&id) {
                    self.drop_conn();
                    return Ok(None);
                }
                let seq = event.seq().max(cursor);
                if seq <= self.cursor {
                    return Ok(None);
                }
                self.cursor = seq;
                self.reconnects = 0;
                Ok(Some(event))
            }
            Ok(Inbound::StreamEnd { cursor, .. }) => {
                if cursor > self.cursor {
                    self.cursor = cursor;
                }
                self.drop_conn();
                Ok(None)
            }
            Ok(_) => {
                self.drop_conn();
                Err(DaemonClientError::Decode)
            }
            Err(DaemonClientError::Transport(IpcError::TimedOut)) => Ok(None),
            Err(err) if err.is_retryable_transport() => {
                self.drop_conn();
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Stop the stream. Further `recv` returns cancelled with the resume cursor.
    pub fn close(&mut self) {
        self.closed = true;
        self.drop_conn();
    }

    fn ensure_subscribed(&mut self) -> Result<(), DaemonClientError> {
        self.client.check_cancel(self.cursor)?;
        if self.closed {
            return Err(DaemonClientError::cancelled(self.cursor));
        }
        if !self.conn_is_none() {
            return Ok(());
        }
        let mut last = DaemonClientError::Transport(IpcError::Io);
        for _ in 0..MAX_RECONNECT_ATTEMPTS {
            self.client.check_cancel(self.cursor)?;
            match self.handshake() {
                Ok(()) => {
                    self.reconnects = 0;
                    return Ok(());
                }
                Err(err) if err.is_retryable_transport() => {
                    last = err;
                    self.drop_conn();
                    if !self.client.sleep_backoff() {
                        return Err(DaemonClientError::cancelled(self.cursor));
                    }
                }
                Err(err) => {
                    self.drop_conn();
                    return Err(err);
                }
            }
        }
        Err(last)
    }

    fn handshake(&mut self) -> Result<(), DaemonClientError> {
        let mut stream = self.client.dial()?;
        let request_id = self.client.next_request_id()?;
        let params = serde_json::json!({
            "session_id": self.session_id,
            "from_seq": self.cursor,
        });
        let body = encode_request(&request_id, "subscribe", params)?;
        write_frame(&mut stream, &body, self.client.limits.max_frame_bytes())
            .map_err(DaemonClientError::Transport)?;
        let reply = read_frame(&mut stream, self.client.limits.max_frame_bytes())
            .map_err(DaemonClientError::Transport)?;
        match decode_inbound(&reply)? {
            Inbound::Ok { id, value } => {
                ensure_id(&id, &request_id)?;
                let ack: SubscribedOk =
                    serde_json::from_value(value).map_err(|_| DaemonClientError::Decode)?;
                if !ack.subscribed {
                    return Err(DaemonClientError::Decode);
                }
                #[cfg(unix)]
                {
                    set_timeouts(&mut stream, Some(STREAM_POLL), self.client.limits)
                        .map_err(DaemonClientError::Transport)?;
                    self.conn = Some(StreamConn { stream, request_id });
                }
                #[cfg(not(unix))]
                {
                    let _ = (stream, request_id);
                }
                Ok(())
            }
            Inbound::Err { id, error } => {
                ensure_id(&id, &request_id)?;
                Err(DaemonClientError::Api(error))
            }
            Inbound::Event { .. } | Inbound::StreamEnd { .. } => Err(DaemonClientError::Decode),
        }
    }

    fn read_stream_frame(&mut self) -> Result<Inbound, DaemonClientError> {
        #[cfg(unix)]
        {
            let conn = self
                .conn
                .as_mut()
                .ok_or(DaemonClientError::Transport(IpcError::Io))?;
            let bytes = read_frame(&mut conn.stream, self.client.limits.max_frame_bytes())
                .map_err(DaemonClientError::Transport)?;
            decode_inbound(&bytes)
        }
        #[cfg(not(unix))]
        {
            Err(DaemonClientError::Transport(IpcError::UnsupportedEndpoint))
        }
    }

    fn event_id_matches(&self, id: &str) -> bool {
        #[cfg(unix)]
        {
            self.conn
                .as_ref()
                .map(|conn| conn.request_id == id)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            false
        }
    }

    fn conn_is_none(&self) -> bool {
        #[cfg(unix)]
        {
            self.conn.is_none()
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn drop_conn(&mut self) {
        #[cfg(unix)]
        {
            self.conn = None;
        }
    }

    fn note_disconnect(&mut self) -> Result<(), DaemonClientError> {
        self.reconnects = self.reconnects.saturating_add(1);
        if self.reconnects > MAX_RECONNECT_ATTEMPTS {
            return Err(DaemonClientError::Transport(IpcError::Io));
        }
        if !self.client.sleep_backoff() {
            return Err(DaemonClientError::cancelled(self.cursor));
        }
        Ok(())
    }
}

impl IpcTurnHandle {
    pub fn session_id(self) -> SessionId {
        self.session_id
    }

    pub fn turn_id(self) -> TurnId {
        self.turn_id
    }

    pub fn seq(self) -> u64 {
        self.seq
    }
}

impl IpcRewindResult {
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub fn through_seq(&self) -> u64 {
        self.through_seq
    }

    pub fn current_seq(&self) -> u64 {
        self.current_seq
    }
}

impl RequestIdempotency {
    /// Classify a daemon method. Unknown methods are treated as non-idempotent.
    pub fn for_method(method: &str) -> Self {
        match method {
            "get_session" | "interrupt" | "rewind" | "subscribe" => Self::Idempotent,
            _ => Self::NonIdempotent,
        }
    }
}

impl DaemonClientError {
    fn cancelled(resume_cursor: u64) -> Self {
        Self::Cancelled { resume_cursor }
    }

    fn is_retryable_transport(&self) -> bool {
        matches!(self, Self::Transport(IpcError::Io | IpcError::TimedOut))
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled { .. } => "cancelled",
            Self::Transport(_) => "transport",
            Self::Api(_) => "api",
            Self::UnknownOutcome { .. } => "unknown_outcome",
            Self::Decode => "decode",
        }
    }
}

impl fmt::Display for DaemonClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { resume_cursor } => {
                write!(f, "daemon client cancelled at seq {resume_cursor}")
            }
            Self::Transport(err) => fmt::Display::fmt(err, f),
            Self::Api(err) => fmt::Display::fmt(err, f),
            Self::UnknownOutcome { method, request_id } => {
                write!(
                    f,
                    "non-idempotent {method} request {request_id} has unknown outcome"
                )
            }
            Self::Decode => f.write_str("malformed ipc response"),
        }
    }
}

impl Error for DaemonClientError {}

impl From<IpcError> for DaemonClientError {
    fn from(err: IpcError) -> Self {
        Self::Transport(err)
    }
}

impl fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonClient")
            .field("endpoint", &self.spec)
            .field("limits", &self.limits)
            .finish()
    }
}

impl fmt::Debug for DaemonEventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonEventStream")
            .field("session_id", &self.session_id)
            .field("cursor", &self.cursor)
            .field("closed", &self.closed)
            .finish()
    }
}

impl Drop for DaemonEventStream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(unix)]
type UnaryStream = std::os::unix::net::UnixStream;

#[cfg(not(unix))]
struct UnaryStream;

fn validate_limits(limits: IpcLimits) -> Result<(), DaemonClientError> {
    if limits.max_frame_bytes() == 0 || limits.max_frame_bytes() > MAX_FRAME_BYTES {
        return Err(DaemonClientError::Transport(IpcError::LimitInvalid));
    }
    if limits.io_timeout().is_zero() {
        return Err(DaemonClientError::Transport(IpcError::LimitInvalid));
    }
    Ok(())
}

fn validate_connect_spec(spec: &ListenSpec) -> Result<(), DaemonClientError> {
    match spec {
        ListenSpec::TcpLoopback { .. } | ListenSpec::WebSocketLoopback { .. } => Err(
            DaemonClientError::Transport(IpcError::NetworkTransportDisabled),
        ),
        ListenSpec::NamedPipe { name } => {
            validate_pipe_name(name).map_err(DaemonClientError::Transport)?;
            Err(DaemonClientError::Transport(IpcError::UnsupportedEndpoint))
        }
        ListenSpec::UnixSocket { path } => {
            validate_socket_path(path).map_err(DaemonClientError::Transport)
        }
    }
}

fn validate_socket_path(path: &Path) -> Result<(), IpcError> {
    if path.as_os_str().is_empty() {
        return Err(IpcError::InvalidPath);
    }
    if path.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(IpcError::InvalidPath);
    }
    if path.file_name().is_none() {
        return Err(IpcError::InvalidPath);
    }
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => return Err(IpcError::InvalidPath),
        }
    }
    Ok(())
}

fn validate_pipe_name(name: &str) -> Result<(), IpcError> {
    if name.is_empty() || name.len() > MAX_PIPE_NAME_BYTES {
        return Err(IpcError::InvalidPath);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(IpcError::InvalidPath);
    }
    Ok(())
}

fn connect_wait(limits: IpcLimits) -> Duration {
    let timeout = limits.io_timeout();
    if timeout < MAX_CONNECT_WAIT {
        timeout
    } else {
        MAX_CONNECT_WAIT
    }
}

fn retryable_connect(err: IpcError) -> bool {
    matches!(err, IpcError::Io | IpcError::TimedOut)
}

fn write_may_have_sent(err: &IpcError) -> bool {
    !matches!(
        err,
        IpcError::FrameTooLarge { .. }
            | IpcError::LimitInvalid
            | IpcError::MalformedFrame
            | IpcError::Cancelled
    )
}

fn encode_request(id: &str, method: &str, params: Value) -> Result<Vec<u8>, DaemonClientError> {
    serde_json::to_vec(&WireRequest {
        schema: IPC_SCHEMA,
        id,
        method,
        params,
    })
    .map_err(|_| DaemonClientError::Transport(IpcError::Internal))
}

fn decode_inbound(body: &[u8]) -> Result<Inbound, DaemonClientError> {
    if std::str::from_utf8(body).is_err() {
        return Err(DaemonClientError::Decode);
    }
    let parsed: WireInbound =
        serde_json::from_slice(body).map_err(|_| DaemonClientError::Decode)?;
    if parsed.schema != IPC_SCHEMA {
        return Err(DaemonClientError::Transport(IpcError::UnsupportedSchema {
            found: parsed.schema,
        }));
    }
    if parsed.id.is_empty() || parsed.id.len() > MAX_REQUEST_ID_BYTES {
        return Err(DaemonClientError::Decode);
    }
    let kinds = u8::from(parsed.ok.is_some())
        + u8::from(parsed.error.is_some())
        + u8::from(parsed.event.is_some())
        + u8::from(parsed.stream_end.is_some());
    if kinds != 1 {
        return Err(DaemonClientError::Decode);
    }
    if let Some(value) = parsed.ok {
        return Ok(Inbound::Ok {
            id: parsed.id,
            value,
        });
    }
    if let Some(error) = parsed.error {
        return Ok(Inbound::Err {
            id: parsed.id,
            error,
        });
    }
    if let Some(event) = parsed.event {
        let cursor = parsed.cursor.unwrap_or_else(|| event.seq());
        return Ok(Inbound::Event {
            id: parsed.id,
            event,
            cursor,
        });
    }
    if let Some(end) = parsed.stream_end {
        return Ok(Inbound::StreamEnd {
            id: parsed.id,
            cursor: end.cursor,
        });
    }
    Err(DaemonClientError::Decode)
}

fn ensure_id(found: &str, expected: &str) -> Result<(), DaemonClientError> {
    if found == expected {
        Ok(())
    } else {
        Err(DaemonClientError::Decode)
    }
}

fn decode_ok<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, DaemonClientError> {
    serde_json::from_value(value).map_err(|_| DaemonClientError::Decode)
}

#[cfg(unix)]
fn try_connect_unix(
    path: &Path,
    limits: IpcLimits,
) -> Result<std::os::unix::net::UnixStream, IpcError> {
    check_connect_target(path)?;
    let mut stream = std::os::unix::net::UnixStream::connect(path).map_err(|_| IpcError::Io)?;
    set_timeouts(&mut stream, Some(limits.io_timeout()), limits)?;
    Ok(stream)
}

#[cfg(unix)]
fn set_timeouts(
    stream: &mut std::os::unix::net::UnixStream,
    read: Option<Duration>,
    limits: IpcLimits,
) -> Result<(), IpcError> {
    // Once the peer has closed (EOF), macOS/BSD reject SO_RCVTIMEO and
    // SO_SNDTIMEO with EINVAL even though the fd is valid and queued bytes
    // remain readable. All durations here are internally controlled (limits
    // are validated, and this stream's options were first set at dial time),
    // so InvalidInput can only reflect that peer-closed state. Re-arming a
    // timeout is flow control, not correctness: the next read still yields
    // the queued bytes and then EOF, driving the normal resume path.
    let tolerate = |res: std::io::Result<()>| match res {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(err) => Err(err),
    };
    tolerate(stream.set_nonblocking(false)).map_err(|_| IpcError::Io)?;
    tolerate(stream.set_read_timeout(read)).map_err(|_| IpcError::Io)?;
    tolerate(stream.set_write_timeout(Some(limits.io_timeout()))).map_err(|_| IpcError::Io)?;
    Ok(())
}

#[cfg(unix)]
fn check_connect_target(path: &Path) -> Result<(), IpcError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(IpcError::Io),
        Err(_) => return Err(IpcError::Io),
    }
    check_socket_private(path)?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.ok_or(IpcError::InvalidPath)?;
    check_dir_private(parent)
}

#[cfg(unix)]
fn check_dir_private(path: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path).map_err(|_| IpcError::Io)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(IpcError::InvalidPath);
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != SOCKET_DIR_MODE {
        return Err(IpcError::InsecurePermissions { mode });
    }
    Ok(())
}

#[cfg(unix)]
fn check_socket_private(path: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path).map_err(|_| IpcError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(IpcError::InsecurePermissions { mode: 0 });
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 || mode & 0o600 != 0o600 {
        return Err(IpcError::InsecurePermissions { mode });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::server::IpcServer;
    use crate::{InProcessKernelClient, InterruptReason};
    use event_ledger::event::{ActorKind, ActorRef, EventKind};
    use protocol::{EventId, ProjectId, TraceId};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        dir: PathBuf,
        sock: PathBuf,
        db: PathBuf,
    }

    impl TempDir {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("rapidlm-ipc-client-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp dir");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("dir mode");
            }
            let sock = dir.join("ipc.sock");
            let db = dir.join("ledger.sqlite");
            Self { dir, sock, db }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.sock);
            let _ = fs::remove_file(&self.db);
            let _ = fs::remove_file(sidecar(&self.db, "-wal"));
            let _ = fs::remove_file(sidecar(&self.db, "-shm"));
            let _ = fs::remove_file(sidecar(&self.db, "-journal"));
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(suffix);
        PathBuf::from(raw)
    }

    fn actor() -> ActorRef {
        ActorRef::new(ActorKind::Human, &EventId::new().to_string()).expect("actor")
    }

    fn create_req() -> CreateSession {
        CreateSession::new(ProjectId::new(), actor(), TraceId::new())
    }

    #[cfg(unix)]
    struct Scripted {
        cancel: CancellationToken,
        join: Option<JoinHandle<()>>,
    }

    #[cfg(unix)]
    impl Drop for Scripted {
        fn drop(&mut self) {
            self.cancel.cancel();
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    #[cfg(unix)]
    fn spawn_scripted<F>(sock: &Path, on_conn: F) -> Scripted
    where
        F: Fn(std::os::unix::net::UnixStream) + Send + Sync + 'static,
    {
        use std::os::unix::fs::PermissionsExt;
        if sock.exists() {
            let _ = fs::remove_file(sock);
        }
        let listener = std::os::unix::net::UnixListener::bind(sock).expect("bind scripted");
        fs::set_permissions(sock, fs::Permissions::from_mode(0o600)).expect("sock mode");
        listener.set_nonblocking(true).expect("nonblocking");
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let on_conn = Arc::new(on_conn);
        let join = thread::spawn(move || {
            let mut workers: Vec<JoinHandle<()>> = Vec::new();
            while !thread_cancel.is_cancelled() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = Arc::clone(&on_conn);
                        if let Ok(handle) = thread::Builder::new()
                            .name("scripted-ipc-conn".to_owned())
                            .spawn(move || handler(stream))
                        {
                            workers.push(handle);
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            for handle in workers {
                let _ = handle.join();
            }
        });
        Scripted {
            cancel,
            join: Some(join),
        }
    }

    #[cfg(unix)]
    fn reply_ok(stream: &mut std::os::unix::net::UnixStream, id: &str, ok: Value) {
        let body = serde_json::to_vec(&serde_json::json!({
            "schema": IPC_SCHEMA,
            "id": id,
            "ok": ok,
        }))
        .expect("ok json");
        write_frame(stream, &body, MAX_FRAME_BYTES).expect("write ok");
    }

    #[cfg(unix)]
    fn reply_event(
        stream: &mut std::os::unix::net::UnixStream,
        id: &str,
        event: &ErasedEventEnvelope,
        cursor: u64,
    ) {
        let body = serde_json::to_vec(&serde_json::json!({
            "schema": IPC_SCHEMA,
            "id": id,
            "event": event,
            "cursor": cursor,
        }))
        .expect("event json");
        write_frame(stream, &body, MAX_FRAME_BYTES).expect("write event");
    }

    #[cfg(unix)]
    fn sample_event(session_id: SessionId, seq: u64) -> ErasedEventEnvelope {
        use event_ledger::event::{EventEnvelope, RecordedAt};
        let recorded = "2020-01-01T00:00:00Z"
            .parse::<RecordedAt>()
            .expect("recorded_at");
        EventEnvelope::new(
            EventId::new(),
            session_id,
            seq,
            recorded,
            actor(),
            TraceId::new(),
            EventKind::SessionCreated,
            protocol::RedactionClass::Project,
            serde_json::json!({ "project_id": ProjectId::new() }),
        )
        .erase()
        .expect("erase")
    }

    #[test]
    fn tcp_and_websocket_are_disabled() {
        let cancel = CancellationToken::new();
        let tcp = DaemonClient::connect(ListenSpec::tcp_loopback(0), cancel.clone());
        assert!(matches!(
            tcp,
            Err(DaemonClientError::Transport(
                IpcError::NetworkTransportDisabled
            ))
        ));
        let ws = DaemonClient::connect(ListenSpec::websocket_loopback(8787), cancel);
        assert!(matches!(
            ws,
            Err(DaemonClientError::Transport(
                IpcError::NetworkTransportDisabled
            ))
        ));
    }

    #[test]
    fn named_pipe_rejects_traversal_and_is_unsupported() {
        let cancel = CancellationToken::new();
        let traversal = DaemonClient::connect(ListenSpec::named_pipe("../evil"), cancel.clone());
        assert!(matches!(
            traversal,
            Err(DaemonClientError::Transport(IpcError::InvalidPath))
        ));
        let named = DaemonClient::connect(ListenSpec::named_pipe("rapidlm-local"), cancel);
        assert!(matches!(
            named,
            Err(DaemonClientError::Transport(IpcError::UnsupportedEndpoint))
        ));
    }

    #[test]
    fn path_traversal_is_rejected() {
        let tmp = TempDir::create();
        let cancel = CancellationToken::new();
        let err = DaemonClient::connect(
            ListenSpec::unix_socket(tmp.dir.join("../escape.sock")),
            cancel,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DaemonClientError::Transport(IpcError::InvalidPath)
        ));
    }

    #[test]
    fn unknown_methods_are_not_classified_idempotent() {
        assert_eq!(
            RequestIdempotency::for_method("get_session"),
            RequestIdempotency::Idempotent
        );
        assert_eq!(
            RequestIdempotency::for_method("subscribe"),
            RequestIdempotency::Idempotent
        );
        assert_eq!(
            RequestIdempotency::for_method("interrupt"),
            RequestIdempotency::Idempotent
        );
        assert_eq!(
            RequestIdempotency::for_method("submit_turn"),
            RequestIdempotency::NonIdempotent
        );
        assert_eq!(
            RequestIdempotency::for_method("create_session"),
            RequestIdempotency::NonIdempotent
        );
        assert_eq!(
            RequestIdempotency::for_method("approve"),
            RequestIdempotency::NonIdempotent
        );
        assert_eq!(
            RequestIdempotency::for_method("not_a_method"),
            RequestIdempotency::NonIdempotent
        );
    }

    #[test]
    fn error_display_omits_payload_bytes() {
        let planted = "secret-canary-token-do-not-echo";
        let err = DaemonClientError::UnknownOutcome {
            method: "submit_turn",
            request_id: "r0000000000000001".to_owned(),
        };
        let shown = err.to_string();
        assert!(!shown.contains(planted));
        assert!(shown.contains("unknown outcome"));
        assert!(!DaemonClientError::Decode.to_string().contains(planted));
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_socket_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::create();
        let listener = std::os::unix::net::UnixListener::bind(&tmp.sock).expect("bind");
        fs::set_permissions(&tmp.sock, fs::Permissions::from_mode(0o666)).expect("widen");
        let cancel = CancellationToken::new();
        let err = DaemonClient::connect(ListenSpec::unix_socket(&tmp.sock), cancel).unwrap_err();
        assert!(
            matches!(
                err,
                DaemonClientError::Transport(IpcError::InsecurePermissions { mode: 0o666 })
            ),
            "{err:?}"
        );
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_parent_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::create();
        let _listener = std::os::unix::net::UnixListener::bind(&tmp.sock).expect("bind");
        fs::set_permissions(&tmp.sock, fs::Permissions::from_mode(0o600)).expect("sock");
        fs::set_permissions(&tmp.dir, fs::Permissions::from_mode(0o777)).expect("widen parent");
        let cancel = CancellationToken::new();
        let err = DaemonClient::connect(ListenSpec::unix_socket(&tmp.sock), cancel).unwrap_err();
        assert!(
            matches!(
                err,
                DaemonClientError::Transport(IpcError::InsecurePermissions { mode: 0o777 })
            ),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_session_and_subscribe_round_trip() {
        let tmp = TempDir::create();
        let kernel = InProcessKernelClient::open(&tmp.db).expect("open kernel");
        let cancel = CancellationToken::new();
        let server = IpcServer::bind(ListenSpec::unix_socket(&tmp.sock), kernel, cancel.clone())
            .expect("bind");
        let _guard = server.spawn().expect("spawn");
        let client =
            DaemonClient::connect(ListenSpec::unix_socket(&tmp.sock), cancel).expect("connect");
        let created = client.create_session(create_req()).expect("create");
        assert_eq!(created.seq(), 1);
        let loaded = client.get_session(created.id()).expect("get");
        assert_eq!(loaded.id(), created.id());
        let mut stream = client
            .subscribe(SubscribeEvents::new(created.id(), 0))
            .expect("subscribe");
        let first = stream.recv().expect("created event");
        assert_eq!(first.kind(), EventKind::SessionCreated);
        assert_eq!(first.seq(), 1);
        assert_eq!(stream.cursor(), 1);
        client
            .interrupt(Interrupt::new(
                created.id(),
                InterruptReason::ClientRequested,
                actor(),
                TraceId::new(),
            ))
            .expect("interrupt");
    }

    #[cfg(unix)]
    #[test]
    fn non_idempotent_disconnect_is_unknown_and_not_replayed() {
        let tmp = TempDir::create();
        let (tx, rx) = mpsc::channel::<String>();
        let hits = Arc::new(AtomicU64::new(0));
        let hits_thread = Arc::clone(&hits);
        let _scripted = spawn_scripted(&tmp.sock, move |mut stream| {
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let body = match read_frame(&mut stream, MAX_FRAME_BYTES) {
                Ok(body) => body,
                Err(_) => return,
            };
            let parsed: Value = serde_json::from_slice(&body).expect("json");
            let id = parsed["id"].as_str().unwrap_or_default().to_owned();
            let method = parsed["method"].as_str().unwrap_or_default().to_owned();
            let _ = tx.send(format!("{method}:{id}"));
            drop(stream);
        });
        let cancel = CancellationToken::new();
        let client =
            DaemonClient::connect(ListenSpec::unix_socket(&tmp.sock), cancel).expect("connect");
        let err = client
            .submit_turn(SubmitTurn::new(
                SessionId::new(),
                1,
                actor(),
                TraceId::new(),
            ))
            .unwrap_err();
        match err {
            DaemonClientError::UnknownOutcome { method, request_id } => {
                assert_eq!(method, "submit_turn");
                assert!(request_id.starts_with('r'));
            }
            other => panic!("expected unknown outcome, got {other:?}"),
        }
        let recorded = rx.recv_timeout(Duration::from_secs(10)).expect("recorded");
        assert!(recorded.starts_with("submit_turn:"));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn idempotent_get_uses_fresh_request_id_on_retry() {
        let tmp = TempDir::create();
        let (tx, rx) = mpsc::channel::<String>();
        let session = SessionId::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let session_ok = session;
        let _scripted = spawn_scripted(&tmp.sock, move |mut stream| {
            let body = match read_frame(&mut stream, MAX_FRAME_BYTES) {
                Ok(body) => body,
                Err(_) => return,
            };
            let parsed: Value = serde_json::from_slice(&body).expect("json");
            let id = parsed["id"].as_str().unwrap_or_default().to_owned();
            let method = parsed["method"].as_str().unwrap_or_default().to_owned();
            seen_thread
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(id.clone());
            let _ = tx.send(id.clone());
            if seen_thread
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
                == 1
            {
                drop(stream);
                return;
            }
            if method == "get_session" {
                reply_ok(
                    &mut stream,
                    &id,
                    serde_json::json!({
                        "schema": 1,
                        "id": session_ok,
                        "project_id": ProjectId::new(),
                        "status": "ready",
                        "active_turn": null,
                        "top_level_goal": null,
                        "active_agents": [],
                        "seq": 1,
                        "created_at": "2020-01-01T00:00:00Z",
                        "updated_at": "2020-01-01T00:00:00Z",
                    }),
                );
            }
        });
        let cancel = CancellationToken::new();
        let client =
            DaemonClient::connect(ListenSpec::unix_socket(&tmp.sock), cancel).expect("connect");
        let loaded = client.get_session(session);
        let first = rx.recv_timeout(Duration::from_secs(10)).expect("first id");
        let second = rx.recv_timeout(Duration::from_secs(10)).expect("second id");
        assert_ne!(first, second, "request ids must not be reused");
        assert!(loaded.is_ok(), "{loaded:?}");
    }

    #[cfg(unix)]
    #[test]
    fn stream_disconnect_resumes_by_cursor_and_dedups() {
        let tmp = TempDir::create();
        let session = SessionId::new();
        let event1 = sample_event(session, 1);
        let event1_dup = event1.clone();
        let event2 = sample_event(session, 2);
        let (from_seq_tx, from_seq_rx) = mpsc::channel::<u64>();
        let first_event = event1.clone();
        let _scripted = spawn_scripted(&tmp.sock, move |mut stream| {
            let body = match read_frame(&mut stream, MAX_FRAME_BYTES) {
                Ok(body) => body,
                Err(_) => return,
            };
            let parsed: Value = serde_json::from_slice(&body).expect("json");
            let id = parsed["id"].as_str().unwrap_or_default().to_owned();
            let method = parsed["method"].as_str().unwrap_or_default().to_owned();
            if method != "subscribe" {
                drop(stream);
                return;
            }
            let from_seq = parsed["params"]["from_seq"].as_u64().unwrap_or(u64::MAX);
            let _ = from_seq_tx.send(from_seq);
            reply_ok(
                &mut stream,
                &id,
                serde_json::json!({ "subscribed": true, "cursor": from_seq }),
            );
            if from_seq == 0 {
                reply_event(&mut stream, &id, &first_event, 1);
                drop(stream);
            } else {
                reply_event(&mut stream, &id, &event1_dup, 1);
                reply_event(&mut stream, &id, &event2, 2);
            }
        });
        let cancel = CancellationToken::new();
        let client =
            DaemonClient::connect(ListenSpec::unix_socket(&tmp.sock), cancel).expect("connect");
        let mut stream = client
            .subscribe(SubscribeEvents::new(session, 0))
            .expect("subscribe");
        let first = stream.recv().expect("seq 1");
        assert_eq!(first.seq(), 1);
        assert_eq!(stream.cursor(), 1);
        let second = stream.recv().expect("seq 2 after resume");
        assert_eq!(second.seq(), 2);
        assert_eq!(stream.cursor(), 2);
        let first_from = from_seq_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("from 0");
        let resume_from = from_seq_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("from 1");
        assert_eq!(first_from, 0);
        assert_eq!(resume_from, 1);
    }

    #[cfg(unix)]
    #[test]
    fn cancel_stops_connect() {
        let tmp = TempDir::create();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = DaemonClient::connect(ListenSpec::unix_socket(&tmp.sock), cancel).unwrap_err();
        assert!(matches!(
            err,
            DaemonClientError::Cancelled { resume_cursor: 0 }
        ));
    }
}
