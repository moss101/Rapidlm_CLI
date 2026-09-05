//! KernelClient-compatible local daemon listener.
//!
//! Default bind is a Unix domain socket (named pipe on Windows). TCP and
//! WebSocket endpoints are refused: they skip filesystem/ACL isolation.
//! Socket files are created owner-only (`0600`) under an owner-only directory
//! (`0700`). Widened permissions fail closed. A malformed client is isolated
//! to its connection.

use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use event_ledger::event::{ActorRef, ErasedEventEnvelope};
use protocol::{
    ApiError, ErrorCode, ProjectId, SessionId, TraceId, TurnId, UNKNOWN_INTERNAL_MESSAGE,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ApprovalDecision, CancellationToken, CreateSession, EventStreamError, ForkSession, Interrupt,
    InterruptReason, KernelClient, ResolveApproval, RewindSession, SessionSnapshot, SubmitTurn,
    SubscribeEvents,
};

/// Wire schema written on every framed request, response, and event.
pub const IPC_SCHEMA: u16 = 1;

/// Hard ceiling for one length-prefixed frame (header not included).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Maximum simultaneous client connections accepted by one listener.
pub const MAX_CONNECTIONS: usize = 32;

/// Maximum UTF-8 bytes accepted in a request id.
pub const MAX_REQUEST_ID_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted in a method name.
pub const MAX_METHOD_BYTES: usize = 64;

/// Maximum bytes accepted in a Unix socket path (macOS `sun_path` including NUL).
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Maximum UTF-8 bytes accepted in a named-pipe identifier.
pub const MAX_PIPE_NAME_BYTES: usize = 64;

/// Owner-only mode applied to the socket file after bind.
pub const SOCKET_FILE_MODE: u32 = 0o600;

/// Owner-only mode required on the socket's immediate parent directory.
pub const SOCKET_DIR_MODE: u32 = 0o700;

const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IO_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_ACCEPT_POLL: Duration = Duration::from_millis(25);
const STREAM_IDLE_POLL: Duration = Duration::from_millis(10);

/// Where the daemon should listen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListenSpec {
    UnixSocket { path: PathBuf },
    NamedPipe { name: String },
    TcpLoopback { port: u16 },
    WebSocketLoopback { port: u16 },
}

/// Explicit resource ceilings. Larger values than the module caps fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcLimits {
    max_frame_bytes: usize,
    max_connections: usize,
    io_timeout: Duration,
    accept_poll: Duration,
}

/// Local IPC server wrapping a [`KernelClient`] implementation.
pub struct IpcServer<C> {
    client: C,
    cancel: CancellationToken,
    auth: Option<std::sync::Arc<auth::DaemonAuth>>,
    limits: IpcLimits,
    endpoint: ListenSpec,
    socket_path: Option<PathBuf>,
    #[cfg(unix)]
    listener: Option<std::os::unix::net::UnixListener>,
}

/// Join handle for a spawned accept loop. Cancel + join on drop; threads are tracked.
pub struct IpcServerGuard {
    cancel: CancellationToken,
    socket_path: Option<PathBuf>,
    join: Option<JoinHandle<Result<(), IpcError>>>,
}

/// Typed listener/transport failure. Display never echoes frame bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcError {
    Cancelled,
    NetworkTransportDisabled,
    UnsupportedEndpoint,
    InvalidPath,
    AddressInUse,
    InsecurePermissions { mode: u32 },
    BindFailed,
    Io,
    TimedOut,
    FrameTooLarge { limit: usize, observed: usize },
    MalformedFrame,
    UnsupportedSchema { found: u16 },
    InvalidRequest,
    LimitInvalid,
    ConnectionLimit,
    AuthRequired,
    Internal,
}

#[derive(Deserialize)]
struct WireRequest {
    schema: u16,
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct WireOk<T: Serialize> {
    schema: u16,
    id: String,
    ok: T,
}

#[derive(Serialize)]
struct WireErr<'a> {
    schema: u16,
    id: &'a str,
    error: &'a ApiError,
}

#[derive(Serialize)]
struct WireEvent<'a> {
    schema: u16,
    id: &'a str,
    event: &'a ErasedEventEnvelope,
    cursor: u64,
}

#[derive(Serialize)]
struct WireStreamEnd<'a> {
    schema: u16,
    id: &'a str,
    stream_end: StreamEndBody,
}

#[derive(Serialize)]
struct StreamEndBody {
    cursor: u64,
}

#[derive(Serialize)]
struct EmptyOk {}

#[derive(Serialize)]
struct SubscribedOk {
    subscribed: bool,
    cursor: u64,
}

#[derive(Serialize)]
struct TurnHandleWire {
    session_id: SessionId,
    turn_id: TurnId,
    seq: u64,
}

#[derive(Serialize)]
struct RewindWire<'a> {
    snapshot: &'a SessionSnapshot,
    through_seq: u64,
    current_seq: u64,
}

#[derive(Deserialize)]
struct CreateSessionParams {
    project_id: ProjectId,
    actor: ActorRef,
    trace_id: TraceId,
}

#[derive(Deserialize)]
struct SessionIdParams {
    #[serde(alias = "id")]
    session_id: SessionId,
}

#[derive(Deserialize)]
struct SubmitTurnParams {
    session_id: SessionId,
    expected_seq: u64,
    actor: ActorRef,
    trace_id: TraceId,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct InterruptParams {
    session_id: SessionId,
    reason: String,
    actor: ActorRef,
    trace_id: TraceId,
}

#[derive(Deserialize)]
struct SubscribeParams {
    session_id: SessionId,
    from_seq: u64,
}

#[derive(Deserialize)]
struct ApproveParams {
    session_id: SessionId,
    expected_seq: u64,
    decision: String,
    actor: ActorRef,
    trace_id: TraceId,
}

#[derive(Deserialize)]
struct ForkParams {
    source: SessionId,
    at_seq: u64,
    actor: ActorRef,
    trace_id: TraceId,
}

#[derive(Deserialize)]
struct RewindParams {
    session_id: SessionId,
    to_seq: u64,
}

struct InflightGuard(Arc<AtomicUsize>);

struct ConnRegistry {
    handles: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl ListenSpec {
    pub fn unix_socket(path: impl Into<PathBuf>) -> Self {
        Self::UnixSocket { path: path.into() }
    }

    pub fn named_pipe(name: impl Into<String>) -> Self {
        Self::NamedPipe { name: name.into() }
    }

    pub fn tcp_loopback(port: u16) -> Self {
        Self::TcpLoopback { port }
    }

    pub fn websocket_loopback(port: u16) -> Self {
        Self::WebSocketLoopback { port }
    }
}

impl IpcLimits {
    pub fn new(
        max_frame_bytes: usize,
        max_connections: usize,
        io_timeout: Duration,
    ) -> Result<Self, IpcError> {
        if max_frame_bytes == 0 || max_frame_bytes > MAX_FRAME_BYTES {
            return Err(IpcError::LimitInvalid);
        }
        if max_connections == 0 || max_connections > MAX_CONNECTIONS {
            return Err(IpcError::LimitInvalid);
        }
        if io_timeout.is_zero() || io_timeout > MAX_IO_TIMEOUT {
            return Err(IpcError::LimitInvalid);
        }
        Ok(Self {
            max_frame_bytes,
            max_connections,
            io_timeout,
            accept_poll: DEFAULT_ACCEPT_POLL,
        })
    }

    pub fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    pub fn max_connections(self) -> usize {
        self.max_connections
    }

    pub fn io_timeout(self) -> Duration {
        self.io_timeout
    }
}

impl Default for IpcLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_connections: MAX_CONNECTIONS,
            io_timeout: DEFAULT_IO_TIMEOUT,
            accept_poll: DEFAULT_ACCEPT_POLL,
        }
    }
}

impl<C> IpcServer<C>
where
    C: KernelClient + Clone + Send + Sync + 'static,
{
    pub fn bind(spec: ListenSpec, client: C, cancel: CancellationToken) -> Result<Self, IpcError> {
        Self::bind_with_limits(spec, client, cancel, IpcLimits::default())
    }

    pub fn bind_with_limits(
        spec: ListenSpec,
        client: C,
        cancel: CancellationToken,
        limits: IpcLimits,
    ) -> Result<Self, IpcError> {
        if cancel.is_cancelled() {
            return Err(IpcError::Cancelled);
        }
        if limits.max_frame_bytes == 0 || limits.max_frame_bytes > MAX_FRAME_BYTES {
            return Err(IpcError::LimitInvalid);
        }
        if limits.max_connections == 0 || limits.max_connections > MAX_CONNECTIONS {
            return Err(IpcError::LimitInvalid);
        }
        if limits.io_timeout.is_zero() || limits.io_timeout > MAX_IO_TIMEOUT {
            return Err(IpcError::LimitInvalid);
        }
        if limits.accept_poll.is_zero() {
            return Err(IpcError::LimitInvalid);
        }

        match spec {
            ListenSpec::TcpLoopback { .. } | ListenSpec::WebSocketLoopback { .. } => {
                Err(IpcError::NetworkTransportDisabled)
            }
            ListenSpec::NamedPipe { name } => {
                validate_pipe_name(&name)?;
                // Safe named-pipe server APIs are not available under
                // `forbid(unsafe_code)` without a reviewed platform crate.
                Err(IpcError::UnsupportedEndpoint)
            }
            ListenSpec::UnixSocket { path } => {
                validate_socket_path(&path)?;
                #[cfg(unix)]
                {
                    Self::bind_unix(path, client, cancel, limits)
                }
                #[cfg(not(unix))]
                {
                    let _ = (client, cancel, limits);
                    Err(IpcError::UnsupportedEndpoint)
                }
            }
        }
    }

    pub fn endpoint(&self) -> &ListenSpec {
        &self.endpoint
    }

    pub fn socket_path(&self) -> Option<&Path> {
        self.socket_path.as_deref()
    }

    pub fn limits(&self) -> IpcLimits {
        self.limits
    }

    /// Run the accept loop on this thread until cancelled or a hard failure.
    pub fn serve(self) -> Result<(), IpcError> {
        #[cfg(unix)]
        {
            self.serve_unix()
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            Err(IpcError::UnsupportedEndpoint)
        }
    }

    /// Spawn a tracked accept thread. Drop/cancel joins it.
    pub fn spawn(self) -> Result<IpcServerGuard, IpcError> {
        let cancel = self.cancel.clone();
        let socket_path = self.socket_path.clone();
        let join = thread::Builder::new()
            .name("rapidlm-ipc-accept".to_owned())
            .spawn(move || self.serve())
            .map_err(|_| IpcError::Internal)?;
        Ok(IpcServerGuard {
            cancel,
            socket_path,
            join: Some(join),
        })
    }

    #[cfg(unix)]
    fn bind_unix(
        path: PathBuf,
        client: C,
        cancel: CancellationToken,
        limits: IpcLimits,
    ) -> Result<Self, IpcError> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        let parent = parent.ok_or(IpcError::InvalidPath)?;
        ensure_private_parent(parent)?;
        if path.exists() {
            return Err(IpcError::AddressInUse);
        }
        let listener = match std::os::unix::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                return Err(IpcError::AddressInUse);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(IpcError::AddressInUse);
            }
            Err(_) => return Err(IpcError::BindFailed),
        };
        if let Err(err) = set_unix_mode(&path, SOCKET_FILE_MODE) {
            let _ = std::fs::remove_file(&path);
            return Err(err);
        }
        if let Err(err) = check_socket_private(&path) {
            let _ = std::fs::remove_file(&path);
            return Err(err);
        }
        Ok(Self {
            client,
            cancel,
            auth: None,
            limits,
            endpoint: ListenSpec::unix_socket(path.clone()),
            socket_path: Some(path),
            listener: Some(listener),
        })
    }

    /// Require OS-user-bound token authentication on every connection.
    /// Unauthenticated clients fail closed before any kernel API runs.
    pub fn with_auth(mut self, auth: std::sync::Arc<auth::DaemonAuth>) -> Self {
        self.auth = Some(auth);
        self
    }

    #[cfg(unix)]
    fn serve_unix(self) -> Result<(), IpcError> {
        let threads = ConnRegistry::new();
        let result = self.accept_loop(&threads);
        self.cancel.cancel();
        threads.join_all();
        result
    }

    #[cfg(unix)]
    fn accept_loop(&self, threads: &ConnRegistry) -> Result<(), IpcError> {
        let listener = self.listener.as_ref().ok_or(IpcError::Internal)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| IpcError::BindFailed)?;
        let inflight = Arc::new(AtomicUsize::new(0));
        loop {
            if self.cancel.is_cancelled() {
                return Err(IpcError::Cancelled);
            }
            if let Some(path) = &self.socket_path {
                check_socket_private(path)?;
                let parent = path.parent().ok_or(IpcError::InvalidPath)?;
                check_dir_private(parent)?;
            }
            threads.reap();
            match listener.accept() {
                Ok((stream, _)) => self.dispatch_connection(stream, &inflight, threads),
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::Interrupted =>
                {
                    thread::sleep(self.limits.accept_poll);
                }
                Err(_) => return Err(IpcError::Io),
            }
        }
    }

    #[cfg(unix)]
    fn dispatch_connection(
        &self,
        stream: std::os::unix::net::UnixStream,
        inflight: &Arc<AtomicUsize>,
        threads: &ConnRegistry,
    ) {
        let prev = inflight.fetch_add(1, Ordering::SeqCst);
        if prev >= self.limits.max_connections {
            inflight.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let client = self.client.clone();
        let cancel = self.cancel.clone();
        let limits = self.limits;
        let auth = self.auth.clone();
        let counter = Arc::clone(inflight);
        match thread::Builder::new()
            .name("rapidlm-ipc-conn".to_owned())
            .spawn(move || {
                let _guard = InflightGuard(counter);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_connection(stream, client, cancel, limits, auth.as_deref());
                }));
            }) {
            Ok(handle) => threads.push(handle),
            Err(_) => {
                inflight.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

impl IpcServerGuard {
    pub fn socket_path(&self) -> Option<&Path> {
        self.socket_path.as_deref()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn wait(mut self) -> Result<(), IpcError> {
        self.cancel.cancel();
        match self.join.take() {
            Some(handle) => match handle.join() {
                Ok(Ok(())) | Ok(Err(IpcError::Cancelled)) => Ok(()),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(IpcError::Internal),
            },
            None => Ok(()),
        }
    }
}

impl Drop for IpcServerGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl<C> Drop for IpcServer<C> {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(path) = &self.socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl<C> fmt::Debug for IpcServer<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpcServer")
            .field("endpoint", &self.endpoint)
            .field("limits", &self.limits)
            .finish()
    }
}

impl ConnRegistry {
    fn new() -> Self {
        Self {
            handles: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<JoinHandle<()>>> {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, handle: JoinHandle<()>) {
        self.lock().push(handle);
    }

    fn reap(&self) {
        let mut handles = self.lock();
        let mut idx = 0;
        while idx < handles.len() {
            if handles[idx].is_finished() {
                let handle = handles.swap_remove(idx);
                let _ = handle.join();
            } else {
                idx += 1;
            }
        }
    }

    fn join_all(&self) {
        let leftover: Vec<JoinHandle<()>> = {
            let mut handles = self.lock();
            handles.drain(..).collect()
        };
        for handle in leftover {
            let _ = handle.join();
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IpcError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::NetworkTransportDisabled => "network_transport_disabled",
            Self::UnsupportedEndpoint => "unsupported_endpoint",
            Self::InvalidPath => "invalid_path",
            Self::AddressInUse => "address_in_use",
            Self::InsecurePermissions { .. } => "insecure_permissions",
            Self::BindFailed => "bind_failed",
            Self::Io => "io",
            Self::TimedOut => "timed_out",
            Self::FrameTooLarge { .. } => "frame_too_large",
            Self::MalformedFrame => "malformed_frame",
            Self::AuthRequired => "auth_required",
            Self::UnsupportedSchema { .. } => "unsupported_schema",
            Self::InvalidRequest => "invalid_request",
            Self::LimitInvalid => "limit_invalid",
            Self::ConnectionLimit => "connection_limit",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthRequired => {
                write!(f, "daemon connection requires a valid auth proof")
            }
            Self::InsecurePermissions { mode } => {
                write!(
                    f,
                    "ipc endpoint permissions are not owner-only (mode {mode:#o})"
                )
            }
            Self::FrameTooLarge { limit, observed } => {
                write!(f, "ipc frame exceeds {limit} bytes (observed {observed})")
            }
            Self::UnsupportedSchema { found } => {
                write!(f, "unsupported ipc schema {found}")
            }
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for IpcError {}

/// Read one length-prefixed frame. The length is rejected before allocation.
pub fn read_frame<R: Read>(reader: &mut R, max_frame_bytes: usize) -> Result<Vec<u8>, IpcError> {
    if max_frame_bytes == 0 || max_frame_bytes > MAX_FRAME_BYTES {
        return Err(IpcError::LimitInvalid);
    }
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).map_err(map_io)?;
    let observed = u32::from_be_bytes(header) as usize;
    if observed == 0 {
        return Err(IpcError::MalformedFrame);
    }
    if observed > max_frame_bytes {
        return Err(IpcError::FrameTooLarge {
            limit: max_frame_bytes,
            observed,
        });
    }
    let mut body = vec![0u8; observed];
    reader.read_exact(&mut body).map_err(map_io)?;
    Ok(body)
}

/// Write one length-prefixed frame. Oversized payloads fail closed.
pub fn write_frame<W: Write>(
    writer: &mut W,
    bytes: &[u8],
    max_frame_bytes: usize,
) -> Result<(), IpcError> {
    if max_frame_bytes == 0 || max_frame_bytes > MAX_FRAME_BYTES {
        return Err(IpcError::LimitInvalid);
    }
    if bytes.is_empty() {
        return Err(IpcError::MalformedFrame);
    }
    if bytes.len() > max_frame_bytes {
        return Err(IpcError::FrameTooLarge {
            limit: max_frame_bytes,
            observed: bytes.len(),
        });
    }
    let len = u32::try_from(bytes.len()).map_err(|_| IpcError::FrameTooLarge {
        limit: max_frame_bytes,
        observed: bytes.len(),
    })?;
    writer.write_all(&len.to_be_bytes()).map_err(map_io)?;
    writer.write_all(bytes).map_err(map_io)?;
    writer.flush().map_err(map_io)?;
    Ok(())
}

#[cfg(unix)]
/// Challenge -> proof -> grant exchange at the head of one connection.
#[cfg(unix)]
fn auth_handshake(
    stream: &mut std::os::unix::net::UnixStream,
    auth: &auth::DaemonAuth,
    cancel: CancellationToken,
    limits: IpcLimits,
) -> Result<Option<auth::ClientGrant>, IpcError> {
    // Wire sizes fixed by auth/src/local_daemon.rs constants.
    const CHALLENGE_ID_BYTES: usize = 16;
    const TOKEN_BYTES: usize = 32;
    use auth::{AuthProof, CancellationToken as AuthCancel};
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    fn unhex(text: &str) -> Option<Vec<u8>> {
        // `text.len()` counts bytes, not chars — a non-ASCII byte (e.g. inside a
        // multi-byte UTF-8 sequence) can still make the length even while landing
        // the `i..i+2` step on a non-char-boundary offset, which panics on slice.
        // Requiring every byte to be an ASCII hex digit guarantees single-byte
        // chars, so every stepped offset is always a valid boundary.
        if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
            .collect()
    }
    if cancel.is_cancelled() {
        return Err(IpcError::Cancelled);
    }
    let auth_cancel = AuthCancel::new();
    let challenge = auth
        .issue_challenge(&auth_cancel)
        .map_err(|_| IpcError::Internal)?;
    let payload = serde_json::json!({
        "schema": 1u16,
        "id": "auth-0",
        "method": "auth.challenge",
        "params": {
            "challenge_id": challenge.id_hex(),
            "nonce": hex(challenge.nonce()),
        }
    });
    let body = serde_json::to_vec(&payload).map_err(|_| IpcError::Internal)?;
    write_frame(stream, &body, limits.max_frame_bytes)?;
    let reply = read_frame(stream, limits.max_frame_bytes)?;
    let value: Value = serde_json::from_slice(&reply).map_err(|_| IpcError::MalformedFrame)?;
    let params = value.get("params").ok_or(IpcError::AuthRequired)?;
    let challenge_hex = params
        .get("challenge_id")
        .and_then(Value::as_str)
        .ok_or(IpcError::AuthRequired)?;
    let response_hex = params
        .get("response")
        .and_then(Value::as_str)
        .ok_or(IpcError::AuthRequired)?;
    let challenge_id = unhex(challenge_hex).ok_or(IpcError::AuthRequired)?;
    let response = unhex(response_hex).ok_or(IpcError::AuthRequired)?;
    if challenge_id.len() != CHALLENGE_ID_BYTES || response.len() != TOKEN_BYTES {
        return Err(IpcError::AuthRequired);
    }
    let mut cid = [0u8; CHALLENGE_ID_BYTES];
    cid.copy_from_slice(&challenge_id);
    let mut resp = [0u8; TOKEN_BYTES];
    resp.copy_from_slice(&response);
    let proof = AuthProof::from_parts(cid, resp);
    let grant = auth
        .authenticate(&proof, &auth_cancel)
        .map_err(|_| IpcError::AuthRequired)?;
    Ok(Some(grant))
}

fn handle_connection<C>(
    mut stream: std::os::unix::net::UnixStream,
    client: C,
    cancel: CancellationToken,
    limits: IpcLimits,
    auth: Option<&auth::DaemonAuth>,
) where
    C: KernelClient,
{
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    if stream.set_read_timeout(Some(limits.io_timeout)).is_err() {
        return;
    }
    if stream.set_write_timeout(Some(limits.io_timeout)).is_err() {
        return;
    }
    // P10-002: authenticated daemons challenge every connection up front.
    // Any malformed/missing/wrong proof fails the connection closed before
    // a single kernel API runs.
    let grant = match auth {
        Some(auth) => match auth_handshake(&mut stream, auth, cancel.clone(), limits) {
            Ok(grant) => grant,
            Err(err) => {
                let _ = write_transport_error(&mut stream, "", err, limits.max_frame_bytes);
                return;
            }
        },
        None => None,
    };
    while !cancel.is_cancelled() {
        match read_frame(&mut stream, limits.max_frame_bytes) {
            Ok(body) => {
                if let Err(err) =
                    handle_request(&mut stream, &client, &cancel, limits, &body, auth, grant.as_ref())
                {
                    match err {
                        IpcError::Cancelled
                        | IpcError::Io
                        | IpcError::TimedOut
                        | IpcError::FrameTooLarge { .. }
                        | IpcError::MalformedFrame => return,
                        other => {
                            if write_transport_error(&mut stream, "", other, limits.max_frame_bytes)
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
            Err(IpcError::Io) | Err(IpcError::TimedOut) | Err(IpcError::Cancelled) => return,
            Err(err @ IpcError::FrameTooLarge { .. }) | Err(err @ IpcError::MalformedFrame) => {
                let _ = write_transport_error(&mut stream, "", err, limits.max_frame_bytes);
                return;
            }
            Err(_) => return,
        }
    }
}

#[cfg(unix)]
fn handle_request<C, W>(
    writer: &mut W,
    client: &C,
    cancel: &CancellationToken,
    limits: IpcLimits,
    body: &[u8],
    auth: Option<&auth::DaemonAuth>,
    grant: Option<&auth::ClientGrant>,
) -> Result<(), IpcError>
where
    C: KernelClient,
    W: Write,
{
    if cancel.is_cancelled() {
        return Err(IpcError::Cancelled);
    }
    let req = match decode_request(body) {
        Ok(req) => req,
        Err(DecodeFailure::Malformed) => return Err(IpcError::MalformedFrame),
        Err(DecodeFailure::Schema { id, found }) => {
            return write_transport_error(
                writer,
                &id,
                IpcError::UnsupportedSchema { found },
                limits.max_frame_bytes,
            );
        }
        Err(DecodeFailure::Invalid { id }) => {
            return write_transport_error(
                writer,
                &id,
                IpcError::InvalidRequest,
                limits.max_frame_bytes,
            );
        }
    };
    // P10-002: the handshake at connect time only proves the grant was valid
    // then — a daemon owner can rotate/revoke the token afterward (`auth::
    // DaemonAuth::issue`) and an already-open connection must not keep
    // running session APIs on the stale grant. Re-validate the same cached
    // grant against the daemon's *current* token on every request, not just
    // once at connect: `authorize_session_api` compares `grant`'s MAC
    // against `self.lock()`'s live token, so a rotated token makes this
    // fail closed exactly like a fresh connection presenting no proof would.
    if let Some(auth) = auth
        && let Some(kind) = session_api_kind(req.method.as_str())
        && auth
            .authorize_session_api(grant, kind, &auth::CancellationToken::new())
            .is_err()
    {
        return write_transport_error(
            writer,
            &req.id,
            IpcError::AuthRequired,
            limits.max_frame_bytes,
        );
    }
    dispatch_method(writer, client, cancel, limits, &req)
}

/// Maps a wire method name onto the [`auth::SessionApiKind`] it must be
/// authorized against. `None` for methods with no session-API gate (there
/// are none today — every dispatched method below has a session-API kind).
#[cfg(unix)]
fn session_api_kind(method: &str) -> Option<auth::SessionApiKind> {
    Some(match method {
        "create_session" => auth::SessionApiKind::Create,
        "get_session" => auth::SessionApiKind::Get,
        "submit_turn" => auth::SessionApiKind::SubmitTurn,
        "interrupt" => auth::SessionApiKind::Interrupt,
        "subscribe" => auth::SessionApiKind::Subscribe,
        "approve" => auth::SessionApiKind::Approve,
        "fork_session" => auth::SessionApiKind::Fork,
        "rewind" => auth::SessionApiKind::Rewind,
        _ => return None,
    })
}

#[cfg(unix)]
fn dispatch_method<C, W>(
    writer: &mut W,
    client: &C,
    cancel: &CancellationToken,
    limits: IpcLimits,
    req: &WireRequest,
) -> Result<(), IpcError>
where
    C: KernelClient,
    W: Write,
{
    match req.method.as_str() {
        "create_session" => {
            let params: CreateSessionParams = decode_params(&req.params)?;
            let call = CreateSession::new(params.project_id, params.actor, params.trace_id);
            respond_result(
                writer,
                &req.id,
                limits,
                call_client(client.create_session(call)),
            )
        }
        "get_session" => {
            let params: SessionIdParams = decode_params(&req.params)?;
            respond_result(
                writer,
                &req.id,
                limits,
                call_client(client.get_session(params.session_id)),
            )
        }
        "submit_turn" => {
            let params: SubmitTurnParams = decode_params(&req.params)?;
            let call = SubmitTurn::new(
                params.session_id,
                params.expected_seq,
                params.actor,
                params.trace_id,
                params.text,
            );
            match call_client(client.submit_turn(call)) {
                Ok(handle) => write_ok(
                    writer,
                    &req.id,
                    TurnHandleWire {
                        session_id: handle.session_id(),
                        turn_id: handle.turn_id(),
                        seq: handle.seq(),
                    },
                    limits.max_frame_bytes,
                ),
                Err(err) => write_error_frame(writer, &req.id, &err, limits.max_frame_bytes),
            }
        }
        "interrupt" => {
            let params: InterruptParams = decode_params(&req.params)?;
            let reason = parse_interrupt_reason(&params.reason)?;
            let call = Interrupt::new(params.session_id, reason, params.actor, params.trace_id);
            match call_client(client.interrupt(call)) {
                Ok(()) => write_ok(writer, &req.id, EmptyOk {}, limits.max_frame_bytes),
                Err(err) => write_error_frame(writer, &req.id, &err, limits.max_frame_bytes),
            }
        }
        "subscribe" => {
            let params: SubscribeParams = decode_params(&req.params)?;
            let call = SubscribeEvents::new(params.session_id, params.from_seq);
            let mut stream = match call_client(client.subscribe(call)) {
                Ok(stream) => stream,
                Err(err) => {
                    return write_error_frame(writer, &req.id, &err, limits.max_frame_bytes);
                }
            };
            write_ok(
                writer,
                &req.id,
                SubscribedOk {
                    subscribed: true,
                    cursor: stream.cursor(),
                },
                limits.max_frame_bytes,
            )?;
            pump_events(writer, &req.id, cancel, limits, &mut stream)
        }
        "approve" => {
            let params: ApproveParams = decode_params(&req.params)?;
            let decision = parse_approval_decision(&params.decision)?;
            let call = ResolveApproval::new(
                params.session_id,
                params.expected_seq,
                decision,
                params.actor,
                params.trace_id,
            );
            match call_client(client.approve(call)) {
                Ok(()) => write_ok(writer, &req.id, EmptyOk {}, limits.max_frame_bytes),
                Err(err) => write_error_frame(writer, &req.id, &err, limits.max_frame_bytes),
            }
        }
        "fork_session" => {
            let params: ForkParams = decode_params(&req.params)?;
            let call =
                ForkSession::new(params.source, params.at_seq, params.actor, params.trace_id);
            respond_result(
                writer,
                &req.id,
                limits,
                call_client(client.fork_session(call)),
            )
        }
        "rewind" => {
            let params: RewindParams = decode_params(&req.params)?;
            let call = RewindSession::new(params.session_id, params.to_seq);
            match call_client(client.rewind(call)) {
                Ok(result) => write_ok(
                    writer,
                    &req.id,
                    RewindWire {
                        snapshot: result.snapshot(),
                        through_seq: result.through_seq(),
                        current_seq: result.current_seq(),
                    },
                    limits.max_frame_bytes,
                ),
                Err(err) => write_error_frame(writer, &req.id, &err, limits.max_frame_bytes),
            }
        }
        _ => write_transport_error(
            writer,
            &req.id,
            IpcError::InvalidRequest,
            limits.max_frame_bytes,
        ),
    }
}

#[cfg(unix)]
fn pump_events<W: Write>(
    writer: &mut W,
    id: &str,
    cancel: &CancellationToken,
    limits: IpcLimits,
    stream: &mut crate::EventStream,
) -> Result<(), IpcError> {
    loop {
        if cancel.is_cancelled() {
            stream.close();
            return write_stream_end(writer, id, stream.cursor(), limits.max_frame_bytes);
        }
        match stream.try_recv() {
            Ok(Some(event)) => {
                let cursor = stream.cursor();
                write_event(writer, id, &event, cursor, limits.max_frame_bytes)?;
            }
            Ok(None) => thread::sleep(STREAM_IDLE_POLL),
            Err(EventStreamError::Cancelled { resume_cursor }) => {
                return write_stream_end(writer, id, resume_cursor, limits.max_frame_bytes);
            }
            Err(_) => {
                return write_transport_error(
                    writer,
                    id,
                    IpcError::Internal,
                    limits.max_frame_bytes,
                );
            }
        }
    }
}

fn respond_result<W: Write>(
    writer: &mut W,
    id: &str,
    limits: IpcLimits,
    result: Result<SessionSnapshot, ApiError>,
) -> Result<(), IpcError> {
    match result {
        Ok(snapshot) => write_ok(writer, id, snapshot, limits.max_frame_bytes),
        Err(err) => write_error_frame(writer, id, &err, limits.max_frame_bytes),
    }
}

fn write_ok<W: Write, T: Serialize>(
    writer: &mut W,
    id: &str,
    ok: T,
    max_frame_bytes: usize,
) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(&WireOk {
        schema: IPC_SCHEMA,
        id: id.to_owned(),
        ok,
    })
    .map_err(|_| IpcError::Internal)?;
    write_frame(writer, &payload, max_frame_bytes)
}

fn write_error_frame<W: Write>(
    writer: &mut W,
    id: &str,
    error: &ApiError,
    max_frame_bytes: usize,
) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(&WireErr {
        schema: IPC_SCHEMA,
        id,
        error,
    })
    .map_err(|_| IpcError::Internal)?;
    write_frame(writer, &payload, max_frame_bytes)
}

fn write_event<W: Write>(
    writer: &mut W,
    id: &str,
    event: &ErasedEventEnvelope,
    cursor: u64,
    max_frame_bytes: usize,
) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(&WireEvent {
        schema: IPC_SCHEMA,
        id,
        event,
        cursor,
    })
    .map_err(|_| IpcError::Internal)?;
    write_frame(writer, &payload, max_frame_bytes)
}

fn write_stream_end<W: Write>(
    writer: &mut W,
    id: &str,
    cursor: u64,
    max_frame_bytes: usize,
) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(&WireStreamEnd {
        schema: IPC_SCHEMA,
        id,
        stream_end: StreamEndBody { cursor },
    })
    .map_err(|_| IpcError::Internal)?;
    write_frame(writer, &payload, max_frame_bytes)
}

fn write_transport_error<W: Write>(
    writer: &mut W,
    id: &str,
    err: IpcError,
    max_frame_bytes: usize,
) -> Result<(), IpcError> {
    let (code, message) = match err {
        IpcError::UnsupportedSchema { .. } => (ErrorCode::ConfigInvalid, "Unsupported IPC schema"),
        IpcError::InvalidRequest => (ErrorCode::ConfigInvalid, "Invalid request"),
        IpcError::MalformedFrame => (ErrorCode::ConfigInvalid, "Malformed IPC frame"),
        IpcError::AuthRequired => (ErrorCode::AuthRequired, "Daemon auth proof required"),
        IpcError::FrameTooLarge { .. } => (ErrorCode::ConfigInvalid, "IPC frame too large"),
        IpcError::Cancelled => (ErrorCode::InternalUnexpected, UNKNOWN_INTERNAL_MESSAGE),
        _ => (ErrorCode::InternalUnexpected, UNKNOWN_INTERNAL_MESSAGE),
    };
    let api = api_error(code, message, TraceId::new());
    write_error_frame(writer, id, &api, max_frame_bytes)
}

enum DecodeFailure {
    Malformed,
    Schema { id: String, found: u16 },
    Invalid { id: String },
}

fn decode_request(body: &[u8]) -> Result<WireRequest, DecodeFailure> {
    if std::str::from_utf8(body).is_err() {
        return Err(DecodeFailure::Malformed);
    }
    let req: WireRequest = serde_json::from_slice(body).map_err(|_| DecodeFailure::Malformed)?;
    if req.schema != IPC_SCHEMA {
        return Err(DecodeFailure::Schema {
            id: req.id,
            found: req.schema,
        });
    }
    if req.id.is_empty() || req.id.len() > MAX_REQUEST_ID_BYTES {
        return Err(DecodeFailure::Invalid { id: req.id });
    }
    if req.method.is_empty() || req.method.len() > MAX_METHOD_BYTES {
        return Err(DecodeFailure::Invalid { id: req.id });
    }
    Ok(req)
}

fn decode_params<T: DeserializeOwned>(params: &Value) -> Result<T, IpcError> {
    serde_json::from_value(params.clone()).map_err(|_| IpcError::InvalidRequest)
}

fn parse_interrupt_reason(value: &str) -> Result<InterruptReason, IpcError> {
    match value {
        "client_requested" => Ok(InterruptReason::ClientRequested),
        _ => Err(IpcError::InvalidRequest),
    }
}

fn parse_approval_decision(value: &str) -> Result<ApprovalDecision, IpcError> {
    match value {
        "approved" => Ok(ApprovalDecision::Approved),
        "denied" => Ok(ApprovalDecision::Denied),
        _ => Err(IpcError::InvalidRequest),
    }
}

fn call_client<T>(
    fut: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    let mut fut = std::pin::pin!(fut);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => Err(api_error(
            ErrorCode::InternalUnexpected,
            UNKNOWN_INTERNAL_MESSAGE,
            TraceId::new(),
        )),
    }
}

fn api_error(code: ErrorCode, message: &'static str, trace: TraceId) -> ApiError {
    match ApiError::new(code, message, trace) {
        Ok(err) => err,
        Err(build) => ApiError::from_unknown(trace, &build),
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

fn map_io(err: io::Error) -> IpcError {
    match err.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => IpcError::TimedOut,
        _ => IpcError::Io,
    }
}

#[cfg(unix)]
fn ensure_private_parent(parent: &Path) -> Result<(), IpcError> {
    match std::fs::symlink_metadata(parent) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                return Err(IpcError::InvalidPath);
            }
            check_dir_private(parent)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(grand) = parent.parent()
                && !grand.as_os_str().is_empty()
                && !grand.exists()
            {
                std::fs::create_dir_all(grand).map_err(|_| IpcError::BindFailed)?;
            }
            std::fs::create_dir(parent).map_err(|_| IpcError::BindFailed)?;
            set_unix_mode(parent, SOCKET_DIR_MODE)?;
            check_dir_private(parent)
        }
        Err(_) => Err(IpcError::BindFailed),
    }
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|_| IpcError::InsecurePermissions { mode: 0 })
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
    use crate::InProcessKernelClient;
    use event_ledger::event::{ActorKind, ActorRef};
    use protocol::{EventId, ProjectId};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempIpc {
        dir: PathBuf,
        db: PathBuf,
        sock: PathBuf,
        client: InProcessKernelClient,
    }

    impl TempIpc {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("rapidlm-ipc-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp ipc dir");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("dir mode");
            }
            let db = dir.join("ledger.sqlite");
            let sock = dir.join("ipc.sock");
            let client = InProcessKernelClient::open(&db).expect("open client");
            Self {
                dir,
                db,
                sock,
                client,
            }
        }
    }

    impl Drop for TempIpc {
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

    fn rpc(method: &str, id: &str, params: Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": IPC_SCHEMA,
            "id": id,
            "method": method,
            "params": params,
        }))
        .expect("rpc json")
    }

    #[cfg(unix)]
    fn unix_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777
    }

    #[cfg(unix)]
    fn connect(path: &Path) -> std::os::unix::net::UnixStream {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("read timeout");
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .expect("write timeout");
                    return stream;
                }
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                Err(err) => panic!("connect {path:?}: {err}"),
            }
        }
    }

    #[cfg(unix)]
    fn exchange(stream: &mut std::os::unix::net::UnixStream, body: &[u8]) -> Value {
        write_frame(stream, body, MAX_FRAME_BYTES).expect("write");
        let reply = read_frame(stream, MAX_FRAME_BYTES).expect("read");
        serde_json::from_slice(&reply).expect("json")
    }

    #[test]
    fn tcp_and_websocket_are_disabled() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let tcp = IpcServer::bind(
            ListenSpec::tcp_loopback(0),
            tmp.client.clone(),
            cancel.clone(),
        );
        assert_eq!(tcp.unwrap_err(), IpcError::NetworkTransportDisabled);
        let ws = IpcServer::bind(
            ListenSpec::websocket_loopback(8787),
            tmp.client.clone(),
            cancel,
        );
        assert_eq!(ws.unwrap_err(), IpcError::NetworkTransportDisabled);
    }

    #[test]
    fn named_pipe_rejects_traversal_and_does_not_bind_without_adapter() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        assert_eq!(
            IpcServer::bind(
                ListenSpec::named_pipe("../evil"),
                tmp.client.clone(),
                cancel.clone(),
            )
            .unwrap_err(),
            IpcError::InvalidPath
        );
        assert_eq!(
            IpcServer::bind(
                ListenSpec::named_pipe("rapidlm-local"),
                tmp.client.clone(),
                cancel
            )
            .unwrap_err(),
            IpcError::UnsupportedEndpoint
        );
    }

    #[test]
    fn path_traversal_is_rejected() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let err = IpcServer::bind(
            ListenSpec::unix_socket(tmp.dir.join("../escape.sock")),
            tmp.client.clone(),
            cancel,
        )
        .unwrap_err();
        assert_eq!(err, IpcError::InvalidPath);
    }

    #[test]
    fn error_display_omits_request_bytes() {
        let planted = "secret-canary-token-do-not-echo";
        let err = IpcError::MalformedFrame;
        let shown = err.to_string();
        assert!(!shown.contains(planted));
        assert_eq!(shown, "malformed_frame");
        let wide = IpcError::InsecurePermissions { mode: 0o666 };
        assert!(!wide.to_string().contains(planted));
    }

    #[test]
    fn invalid_limits_fail_closed() {
        assert_eq!(
            IpcLimits::new(0, 1, Duration::from_secs(1)).unwrap_err(),
            IpcError::LimitInvalid
        );
        assert_eq!(
            IpcLimits::new(MAX_FRAME_BYTES + 1, 1, Duration::from_secs(1)).unwrap_err(),
            IpcError::LimitInvalid
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_is_owner_only_and_unlinked_on_drop() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .expect("bind");
        assert_eq!(unix_mode(&tmp.sock), SOCKET_FILE_MODE);
        assert_eq!(unix_mode(&tmp.dir), SOCKET_DIR_MODE);
        assert!(tmp.sock.exists());
        drop(server);
        assert!(!tmp.sock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_parent_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempIpc::create();
        fs::set_permissions(&tmp.dir, fs::Permissions::from_mode(0o777)).expect("widen parent");
        let cancel = CancellationToken::new();
        let err = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .unwrap_err();
        assert_eq!(err, IpcError::InsecurePermissions { mode: 0o777 });
        assert!(!tmp.sock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn widened_socket_mode_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .expect("bind");
        fs::set_permissions(&tmp.sock, fs::Permissions::from_mode(0o666)).expect("widen");
        let err = server.serve().unwrap_err();
        assert_eq!(err, IpcError::InsecurePermissions { mode: 0o666 });
    }

    #[cfg(unix)]
    #[test]
    fn create_session_round_trip_and_subscribe() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .expect("bind");
        let _guard = server.spawn().expect("spawn");
        let mut stream = connect(&tmp.sock);

        let project = ProjectId::new();
        let created = exchange(
            &mut stream,
            &rpc(
                "create_session",
                "c1",
                serde_json::json!({
                    "project_id": project,
                    "actor": actor(),
                    "trace_id": TraceId::new(),
                }),
            ),
        );
        assert_eq!(created["schema"], IPC_SCHEMA);
        assert_eq!(created["id"], "c1");
        assert!(created.get("error").is_none());
        let session_id = created["ok"]["id"].as_str().expect("session id").to_owned();
        assert_eq!(created["ok"]["seq"], 1);

        let loaded = exchange(
            &mut stream,
            &rpc(
                "get_session",
                "g1",
                serde_json::json!({ "session_id": session_id }),
            ),
        );
        assert_eq!(loaded["ok"]["id"], session_id);

        write_frame(
            &mut stream,
            &rpc(
                "subscribe",
                "s1",
                serde_json::json!({ "session_id": session_id, "from_seq": 0 }),
            ),
            MAX_FRAME_BYTES,
        )
        .expect("subscribe write");
        let ack: Value =
            serde_json::from_slice(&read_frame(&mut stream, MAX_FRAME_BYTES).expect("ack"))
                .expect("ack json");
        assert_eq!(ack["ok"]["subscribed"], true);
        let event: Value =
            serde_json::from_slice(&read_frame(&mut stream, MAX_FRAME_BYTES).expect("event"))
                .expect("event json");
        assert_eq!(event["event"]["kind"], "session.created");
        assert_eq!(event["cursor"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn malformed_client_does_not_crash_daemon() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .expect("bind");
        let _guard = server.spawn().expect("spawn");

        let mut bad = connect(&tmp.sock);
        bad.write_all(b"\x00\x00\x00\x05!!!!!\xffsecret-canary-token-do-not-echo")
            .expect("garbage");
        let _ = bad.flush();
        drop(bad);

        let mut huge = connect(&tmp.sock);
        let too_big = (MAX_FRAME_BYTES as u32).saturating_add(1).to_be_bytes();
        huge.write_all(&too_big).expect("len");
        huge.write_all(&[0x41]).expect("byte");
        let _ = huge.flush();
        // Oversized frames close the connection; the length is not allocated.
        let _ = read_frame(&mut huge, MAX_FRAME_BYTES);
        drop(huge);

        let mut good = connect(&tmp.sock);
        let created = exchange(
            &mut good,
            &rpc(
                "create_session",
                "ok1",
                serde_json::json!({
                    "project_id": ProjectId::new(),
                    "actor": actor(),
                    "trace_id": TraceId::new(),
                }),
            ),
        );
        assert!(created.get("ok").is_some(), "{created}");
        assert!(created.get("error").is_none());

        let unknown = exchange(&mut good, &rpc("not_a_method", "u1", serde_json::json!({})));
        assert_eq!(unknown["error"]["code"], "config.invalid");
        assert!(!unknown.to_string().contains("secret-canary"));
    }

    #[cfg(unix)]
    #[test]
    fn cancel_stops_accept_loop() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .expect("bind");
        let guard = server.spawn().expect("spawn");
        guard.cancel();
        guard.wait().expect("join");
    }


    use auth::{DaemonAuth, DaemonTokenHandle};

    static AUTH_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_runtime(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-ipc-auth-{tag}-{}-{}",
            std::process::id(),
            AUTH_TEMP_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("runtime dir");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode");
        }
        dir
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn unauthenticated_connection_fails_closed_before_any_kernel_api() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let runtime = temp_runtime("reject");
        let auth_cancel = auth::CancellationToken::new();
        let daemon = DaemonAuth::open(&runtime, &auth_cancel).expect("open");
        let _token: DaemonTokenHandle = daemon.issue(&auth_cancel).expect("issue");
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .expect("bind")
        .with_auth(std::sync::Arc::new(daemon));
        let _guard = server.spawn().expect("spawn");
        let mut stream = connect(&tmp.sock);

        // The daemon opens with an auth.challenge frame.
        let challenge_frame =
            read_frame(&mut stream, MAX_FRAME_BYTES).expect("challenge frame");
        let challenge: Value = serde_json::from_slice(&challenge_frame).unwrap();
        assert_eq!(challenge["method"], "auth.challenge");

        // Fire a session API request INSTEAD of the proof.
        let reply = exchange(
            &mut stream,
            &rpc(
                "create_session",
                "c1",
                serde_json::json!({
                    "project_id": ProjectId::new(),
                    "actor": actor(),
                    "trace_id": TraceId::new(),
                }),
            ),
        );
        let err = reply.get("error").expect("error frame");
        assert_eq!(err["code"], "auth.required");
        // The connection is then closed by the daemon.
        assert!(
            read_frame(&mut stream, MAX_FRAME_BYTES).is_err(),
            "connection must not survive a rejected handshake"
        );
    }

    #[test]
    fn non_ascii_hex_proof_fails_closed_without_panicking() {
        let tmp = TempIpc::create();
        let cancel = CancellationToken::new();
        let runtime = temp_runtime("nonascii");
        let auth_cancel = auth::CancellationToken::new();
        let daemon = DaemonAuth::open(&runtime, &auth_cancel).expect("open");
        let _token: DaemonTokenHandle = daemon.issue(&auth_cancel).expect("issue");
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            cancel,
        )
        .expect("bind")
        .with_auth(std::sync::Arc::new(daemon));
        let _guard = server.spawn().expect("spawn");
        let mut stream = connect(&tmp.sock);

        let challenge_frame = read_frame(&mut stream, MAX_FRAME_BYTES).expect("challenge frame");
        let challenge_value: Value = serde_json::from_slice(&challenge_frame).expect("json");
        let cid = challenge_value["params"]["challenge_id"]
            .as_str()
            .expect("cid")
            .to_owned();

        // "a中" is 4 bytes (even length passes a length-only check) but the
        // second stepped offset lands inside the multi-byte '中' sequence.
        let bogus_proof = serde_json::json!({
            "schema": 1u16,
            "id": "auth-0",
            "params": { "challenge_id": cid, "response": "a中" }
        });
        write_frame(&mut stream, &serde_json::to_vec(&bogus_proof).unwrap(), MAX_FRAME_BYTES)
            .unwrap();
        let verdict =
            read_frame(&mut stream, MAX_FRAME_BYTES).expect("clean verdict frame, not a dropped connection");
        let verdict: Value = serde_json::from_slice(&verdict).unwrap();
        assert_eq!(verdict["error"]["code"], "auth.required");
    }

    #[test]
    fn wrong_proof_fails_closed_and_correct_proof_serves_requests() {
        let tmp = TempIpc::create();
        let _cancel = CancellationToken::new();
        let runtime = temp_runtime("happy");
        let auth_cancel = auth::CancellationToken::new();
        let daemon = DaemonAuth::open(&runtime, &auth_cancel).expect("open");
        let _handle: DaemonTokenHandle = daemon.issue(&auth_cancel).expect("issue");
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            CancellationToken::new(),
        )
        .expect("bind")
        .with_auth(std::sync::Arc::new(daemon));
        let _guard = server.spawn().expect("spawn");

        // Wrong proof first: rejected.
        let mut bad = connect(&tmp.sock);
        let challenge_frame =
            read_frame(&mut bad, MAX_FRAME_BYTES).expect("challenge frame");
        let challenge_value: Value =
            serde_json::from_slice(&challenge_frame).expect("json");
        let cid = challenge_value["params"]["challenge_id"].as_str().expect("cid");
        assert_eq!(cid.len(), 32, "16-byte challenge id hex");
        let bogus_proof = serde_json::json!({
            "schema": 1u16,
            "id": "auth-0",
            "params": { "challenge_id": cid, "response": "ab".repeat(32) }
        });
        write_frame(&mut bad, &serde_json::to_vec(&bogus_proof).unwrap(), MAX_FRAME_BYTES)
            .unwrap();
        let verdict = read_frame(&mut bad, MAX_FRAME_BYTES).expect("verdict");
        let verdict: Value = serde_json::from_slice(&verdict).unwrap();
        assert_eq!(verdict["error"]["code"], "auth.required");
        drop(bad);

        // Correct proof: challenge -> prove -> served.
        let mut good = connect(&tmp.sock);
        let challenge_frame =
            read_frame(&mut good, MAX_FRAME_BYTES).expect("challenge frame");
        let challenge_value: Value =
            serde_json::from_slice(&challenge_frame).expect("json");
        let cid_hex = challenge_value["params"]["challenge_id"]
            .as_str()
            .expect("cid")
            .to_owned();
        let nonce_hex = challenge_value["params"]["nonce"]
            .as_str()
            .expect("nonce")
            .to_owned();
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&unhex(&cid_hex));
        let mut nonce_bytes = [0u8; 32];
        nonce_bytes.copy_from_slice(&unhex(&nonce_hex));
        let challenge = auth::AuthChallenge::from_parts(id_bytes, nonce_bytes);
        let client_auth = auth::LocalDaemonClient::open(&runtime, &auth::CancellationToken::new()).expect("client open");
        let proof = client_auth.prove(&challenge, &auth::CancellationToken::new()).expect("prove");
        let proof_body = serde_json::json!({
            "schema": 1u16,
            "id": "auth-0",
            "params": {
                "challenge_id": proof.challenge_id_hex(),
                "response": proof.response_hex(),
            }
        });
        write_frame(
            &mut good,
            &serde_json::to_vec(&proof_body).unwrap(),
            MAX_FRAME_BYTES,
        )
        .expect("write proof");
        let created = exchange(
            &mut good,
            &rpc(
                "create_session",
                "c2",
                serde_json::json!({
                    "project_id": ProjectId::new(),
                    "actor": actor(),
                    "trace_id": TraceId::new(),
                }),
            ),
        );
        assert!(created.get("error").is_none(), "authenticated call succeeds");
    }

    #[test]
    fn rotating_the_token_revokes_an_already_open_connections_session_apis() {
        // The handshake only proves the grant was valid *at connect time*.
        // A daemon owner can rotate the token afterward (DaemonAuth::issue)
        // and an already-authenticated connection must not keep serving
        // session APIs on the now-stale grant.
        let tmp = TempIpc::create();
        let runtime = temp_runtime("rotate");
        let auth_cancel = auth::CancellationToken::new();
        let daemon = std::sync::Arc::new(DaemonAuth::open(&runtime, &auth_cancel).expect("open"));
        let _handle: DaemonTokenHandle = daemon.issue(&auth_cancel).expect("issue");
        let server = IpcServer::bind(
            ListenSpec::unix_socket(&tmp.sock),
            tmp.client.clone(),
            CancellationToken::new(),
        )
        .expect("bind")
        .with_auth(daemon.clone());
        let _guard = server.spawn().expect("spawn");

        let mut stream = connect(&tmp.sock);
        let challenge_frame = read_frame(&mut stream, MAX_FRAME_BYTES).expect("challenge frame");
        let challenge_value: Value = serde_json::from_slice(&challenge_frame).expect("json");
        let cid_hex = challenge_value["params"]["challenge_id"]
            .as_str()
            .expect("cid")
            .to_owned();
        let nonce_hex = challenge_value["params"]["nonce"]
            .as_str()
            .expect("nonce")
            .to_owned();
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&unhex(&cid_hex));
        let mut nonce_bytes = [0u8; 32];
        nonce_bytes.copy_from_slice(&unhex(&nonce_hex));
        let challenge = auth::AuthChallenge::from_parts(id_bytes, nonce_bytes);
        let client_auth =
            auth::LocalDaemonClient::open(&runtime, &auth::CancellationToken::new()).expect("client open");
        let proof = client_auth
            .prove(&challenge, &auth::CancellationToken::new())
            .expect("prove");
        let proof_body = serde_json::json!({
            "schema": 1u16,
            "id": "auth-0",
            "params": {
                "challenge_id": proof.challenge_id_hex(),
                "response": proof.response_hex(),
            }
        });
        write_frame(&mut stream, &serde_json::to_vec(&proof_body).unwrap(), MAX_FRAME_BYTES)
            .expect("write proof");

        // The freshly authenticated connection can call a session API.
        let created = exchange(
            &mut stream,
            &rpc(
                "create_session",
                "c1",
                serde_json::json!({
                    "project_id": ProjectId::new(),
                    "actor": actor(),
                    "trace_id": TraceId::new(),
                }),
            ),
        );
        assert!(created.get("error").is_none(), "call succeeds before rotation");

        // Rotate the token — the daemon owner revoking/re-issuing while the
        // connection stays open, exactly as `DaemonAuth::issue`'s own
        // rotation test exercises.
        let _rotated: DaemonTokenHandle = daemon.issue(&auth_cancel).expect("rotate");

        // The same still-open connection must now be rejected, not keep
        // running session APIs on the stale grant.
        let rejected = exchange(
            &mut stream,
            &rpc(
                "create_session",
                "c2",
                serde_json::json!({
                    "project_id": ProjectId::new(),
                    "actor": actor(),
                    "trace_id": TraceId::new(),
                }),
            ),
        );
        let err = rejected.get("error").expect("must be rejected after rotation");
        assert_eq!(err["code"], "auth.required");
    }

}
