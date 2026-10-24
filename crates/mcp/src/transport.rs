//! MCP client transport and session handshake.
//!
//! Stdio is newline-delimited JSON-RPC. Remote HTTP is streamable-HTTP and
//! must pass `net.connect` scope plus egress authorize/consume before I/O.
//! Auth tokens are [`SecretRef`] handles; plaintext is never stored here.
//! Server frames are untrusted data (T-005, T-007, T-012).

use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{self, Debug};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use auth::{SecretRef, SecretTarget};
use capability_broker::{
    CancellationToken, CanonicalNetHost, CanonicalNetworkTarget, Capability, MAX_URL_BYTES,
    NetworkIntent, NetworkNormalizeError, NetworkResolver, NetworkScope, ResourceDescriptor,
    normalize_network,
};
use protocol::{ApiError, ErrorCode, JobId, TraceId, UNKNOWN_INTERNAL_MESSAGE};
use security::{
    ConsumedConnect, EgressError, EgressOutcome, EgressProxy, NetworkClient, authorize_connect,
};
use serde_json::{Map, Value};

/// Target MCP specification revision offered by RapidLM.
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// Prior common handshake accepted by the compatibility shim.
pub const MCP_PRIOR_PROTOCOL_VERSION: &str = "2025-06-18";

/// Maximum accepted JSON-RPC / HTTP body bytes. Larger output is rejected.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Default I/O deadline for one send or receive.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on a configured I/O deadline: the longest a single tool call may
/// be given (a handshake is capped lower by its caller).
pub const MAX_IO_TIMEOUT: Duration = Duration::from_secs(600);

/// How long the best-effort notice that a call was abandoned may take.
const CANCEL_NOTICE_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum UTF-8 bytes for `clientInfo` / `serverInfo` name.
pub const MAX_IMPLEMENTATION_NAME_BYTES: usize = 128;

/// Maximum UTF-8 bytes for `clientInfo` / `serverInfo` version.
pub const MAX_IMPLEMENTATION_VERSION_BYTES: usize = 64;

/// Maximum UTF-8 bytes for an HTTP `Mcp-Session-Id`.
pub const MAX_MCP_SESSION_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes for a streamable-HTTP path.
pub const MAX_HTTP_PATH_BYTES: usize = 1024;

/// Maximum redirect hops followed after a new egress authorization.
pub const MAX_HTTP_REDIRECTS: u8 = 8;

/// Secret-target prefix bound to a single authorized HTTP origin.
pub const SECRET_TARGET_PREFIX: &str = "mcp.http.auth";

/// Most bytes of one `text/event-stream` reply body. Each event inside it is
/// still bounded by the transport's own frame bound.
pub const MAX_EVENT_STREAM_BYTES: usize = 256 * 1024;

/// Most bytes of a `WWW-Authenticate` header that is kept and parsed; a longer
/// value is dropped whole and the challenge reads as empty. (A line over the
/// HTTP reader's own header-line bound fails the whole response earlier.)
pub const MAX_CHALLENGE_BYTES: usize = 2048;

/// Most bytes of any one parsed challenge parameter.
const MAX_CHALLENGE_PARAM_BYTES: usize = 1024;

const JSONRPC_VERSION: &str = "2.0";
const INITIALIZE_METHOD: &str = "initialize";
const INITIALIZED_METHOD: &str = "notifications/initialized";
const CANCEL_STRIDE: usize = 64;

/// Poll stride while [`StdioTransport::recv_frame`] waits on its background
/// reader thread — bounds how quickly a cancellation or deadline is noticed
/// without busy-spinning.
const RECV_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Which wire the session is speaking.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TransportKind {
    Stdio,
    StreamableHttp,
}

/// Negotiated MCP protocol revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProtocolVersion {
    V2026_07_28,
    V2025_06_18,
}

/// Bounded client or server implementation identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImplementationInfo {
    name: String,
    version: String,
}

/// Capabilities RapidLM offers during `initialize`. These are not grants.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientCapabilities {
    roots: bool,
    /// The client answers `elicitation/create` requests during a call.
    elicitation: bool,
}

/// Server-advertised features recorded at handshake. Not RapidLM privileges.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerCapabilities {
    tools: bool,
    resources: bool,
    prompts: bool,
    logging: bool,
    completions: bool,
}

/// Opaque HTTP session identifier from `Mcp-Session-Id`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct McpSessionId(String);

/// Recorded result of [`McpSession::initialize`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedHandshake {
    protocol_version: ProtocolVersion,
    client_capabilities: ClientCapabilities,
    server_capabilities: ServerCapabilities,
    server_info: ImplementationInfo,
    session_id: Option<McpSessionId>,
}

/// Explicit time and size bounds for one transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoBounds {
    max_frame_bytes: usize,
    timeout: Duration,
}

/// Credential handle scoped to one MCP HTTP origin.
#[derive(Clone, Eq, PartialEq)]
pub struct HttpAuthScope {
    handle: SecretRef,
    target: SecretTarget,
}

/// Inputs required to open streamable HTTP. URL is untrusted until normalize.
pub struct HttpConnectRequest {
    url: String,
    capability: Capability,
    resource: ResourceDescriptor,
    credential: Option<HttpAuthScope>,
    bounds: IoBounds,
}

/// Outbound streamable-HTTP exchange. Authorization header is a handle only.
pub struct HttpRequest<'a> {
    target: &'a CanonicalNetworkTarget,
    path: &'a str,
    session_id: Option<&'a McpSessionId>,
    protocol_version: ProtocolVersion,
    credential: Option<&'a SecretRef>,
    body: &'a [u8],
}

impl HttpRequest<'_> {
    /// Read-only seams for out-of-crate [`StreamableHttpIo`] implementations:
    /// the fields are intentionally not constructible outside the crate (an
    /// exchange can only run with an authorized request the transport built),
    /// but a production io adapter needs the request's pieces to make the
    /// real HTTP call. `body` and `session_id` already had public readers.
    pub fn url(&self) -> String {
        if self.path.starts_with('/') {
            origin_url_for(self.target) + self.path
        } else {
            origin_url_for(self.target) + "/" + self.path
        }
    }
}

/// Public helper: canonical origin -> `scheme://host:port`.
pub fn origin_url_for(target: &CanonicalNetworkTarget) -> String {
    origin_url(target)
}

/// Inbound streamable-HTTP exchange. `Location` is untrusted.
pub struct HttpResponse {
    status: u16,
    session_id: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
    content_type: Option<String>,
    www_authenticate: Option<String>,
}

/// Byte-level MCP transport. Implementations must honor cancel and frame caps.
pub trait McpTransport {
    fn kind(&self) -> TransportKind;

    fn send_frame(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), TransportError>;

    fn recv_frame(&mut self, cancel: &CancellationToken) -> Result<Vec<u8>, TransportError>;

    /// Change the deadline of each later send or receive. A transport with no
    /// deadline of its own keeps this default.
    fn set_io_timeout(&mut self, _timeout: Duration) -> Result<(), TransportError> {
        Ok(())
    }

    /// The deadline of each send or receive now, when the transport has one.
    fn io_timeout(&self) -> Option<Duration> {
        None
    }

    /// One request/response. Stdio is send+recv; HTTP is a single authorized POST.
    fn call(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, TransportError> {
        self.send_frame(bytes, cancel)?;
        self.recv_frame(cancel)
    }

    fn session_id(&self) -> Option<&McpSessionId> {
        None
    }

    /// The challenge of the most recent `401`/`403`, if the last exchange
    /// ended in one. Only streamable HTTP has any.
    fn auth_challenge(&self) -> Option<&AuthChallenge> {
        None
    }

    fn close(&mut self, cancel: &CancellationToken) -> Result<(), TransportError>;

    /// One id-matched request/response. The default is newline framing:
    /// send, then receive until the id-matched reply arrives, discarding
    /// interleaved server notifications. There is deliberately no fixed cap
    /// on how many are skipped: a well-behaved server can emit any number of
    /// `notifications/progress` frames during one call, and giving up early
    /// would leave the real response unread, desynchronizing every later
    /// call on the session. `recv_frame` itself bounds a dead connection
    /// (cancellation, I/O timeout, the peer closing). A transport
    /// whose request and response share one exchange (streamable HTTP's
    /// single POST) overrides this.
    fn exchange_skipping_notifications(
        &mut self,
        request: &[u8],
        expected_id: u64,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, TransportError> {
        self.send_frame(request, cancel)?;
        loop {
            let frame = self.recv_frame(cancel)?;
            if has_response_id(&frame, expected_id) {
                return Ok(frame);
            }
        }
    }
}

/// Performs one authorized streamable-HTTP POST. Callers must pass a consumed
/// connect lease; this trait is not a privilege grant.
pub trait StreamableHttpIo {
    fn exchange(
        &mut self,
        request: &HttpRequest<'_>,
        grant: &ConsumedConnect,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<HttpResponse, TransportError>;
}

/// Newline-delimited JSON-RPC over already-opened supervised pipes.
///
/// Receiving runs on a dedicated background thread: a blocked `Read::read`
/// on a real pipe cannot be interrupted by a cancellation flag or a
/// deadline, only waited on, so `recv_frame` bounds its *wait* on the
/// resulting channel instead of the read itself. A hung peer leaves the
/// reader thread blocked until the pipe eventually closes or errors, but
/// `recv_frame` still returns promptly on cancellation or `IoBounds::timeout`.
pub struct StdioTransport<R, W> {
    frames: mpsc::Receiver<Result<Vec<u8>, TransportError>>,
    _reader: PhantomData<R>,
    writer: W,
    job_id: Option<JobId>,
    bounds: IoBounds,
    closed: bool,
}

/// Streamable-HTTP client. Every POST re-authorizes egress (one-use leases).
pub struct StreamableHttpTransport<I, R> {
    io: I,
    resolver: R,
    proxy: EgressProxy,
    origin_url: String,
    path: String,
    origin: CanonicalNetworkTarget,
    capability: Capability,
    resource: ResourceDescriptor,
    credential: Option<HttpAuthScope>,
    bounds: IoBounds,
    session_id: Option<McpSessionId>,
    closed: bool,
    /// Frames of the latest POST reply not yet read. A POST answers within
    /// its own response, so `send_frame` queues what came back and
    /// `recv_frame` hands it out, which lets the stdio-shaped call loops run
    /// over HTTP unchanged.
    inbound: VecDeque<Vec<u8>>,
    challenge: Option<AuthChallenge>,
}

/// In-memory framed transport for tests and scripted servers.
pub struct LoopbackTransport {
    kind: TransportKind,
    inbound: VecDeque<Vec<u8>>,
    outbound: Vec<Vec<u8>>,
    session_id: Option<McpSessionId>,
    bounds: IoBounds,
    closed: bool,
}

/// MCP client session. Business catalog/trust state lives outside this type.
pub struct McpSession<T> {
    transport: T,
    client_info: ImplementationInfo,
    client_capabilities: ClientCapabilities,
    next_id: u64,
    handshake: Option<NegotiatedHandshake>,
    closed: bool,
}

/// One tool advertised by a server in `tools/list`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Result of `tools/call`: concatenated text content plus the error flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpToolCallOutput {
    pub text: String,
    pub is_error: bool,
}

/// Typed transport / handshake failure. Display never echoes raw frames/URLs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TransportError {
    Cancelled,
    Timeout,
    FrameTooLarge,
    InvalidFrame,
    InvalidBounds,
    InvalidIdent,
    UnsupportedProtocolVersion,
    HandshakeFailed,
    NetworkDenied,
    NetworkScopeDenied,
    CredentialScopeDenied,
    CapabilityDenied,
    NotInitialized,
    AlreadyInitialized,
    ToolFailed,
    Closed,
    RedirectDenied,
    Io,
    /// The server answered `401`: it wants credentials this client did not
    /// send. The parsed challenge is on [`McpTransport::auth_challenge`].
    Unauthorized,
    /// The server answered `403` with `error="insufficient_scope"`.
    InsufficientScope,
}

/// What a `WWW-Authenticate` challenge asked for. Every field is untrusted
/// server text: bounded, free of control characters, and never logged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthChallenge {
    /// URL of the protected-resource metadata document, if the server named one.
    pub resource_metadata: Option<String>,
    /// Space-separated scopes the server asked for.
    pub scope: Option<String>,
    /// The `error` parameter (`invalid_token`, `insufficient_scope`, ...).
    pub error: Option<String>,
}

impl ProtocolVersion {
    pub const TARGET: Self = Self::V2026_07_28;

    pub const SUPPORTED: &'static [Self] = &[Self::V2026_07_28, Self::V2025_06_18];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V2026_07_28 => MCP_PROTOCOL_VERSION,
            Self::V2025_06_18 => MCP_PRIOR_PROTOCOL_VERSION,
        }
    }

    /// Fail closed unless the server named a supported revision.
    pub fn parse(value: &str) -> Result<Self, TransportError> {
        for version in Self::SUPPORTED {
            if version.as_str() == value {
                return Ok(*version);
            }
        }
        Err(TransportError::UnsupportedProtocolVersion)
    }
}

impl ImplementationInfo {
    pub fn new(name: &str, version: &str) -> Result<Self, TransportError> {
        Ok(Self {
            name: parse_ident(name, MAX_IMPLEMENTATION_NAME_BYTES)?,
            version: parse_ident(version, MAX_IMPLEMENTATION_VERSION_BYTES)?,
        })
    }

    pub fn rapidlm() -> Self {
        Self {
            name: "rapidlm".to_owned(),
            version: "0.1.0".to_owned(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

impl ClientCapabilities {
    pub fn new(roots: bool) -> Self {
        Self {
            roots,
            elicitation: false,
        }
    }

    /// Declare that server requests for input (`elicitation/create`) are
    /// answered during a call.
    pub fn with_elicitation(mut self) -> Self {
        self.elicitation = true;
        self
    }

    pub fn elicitation(&self) -> bool {
        self.elicitation
    }

    pub fn roots(&self) -> bool {
        self.roots
    }
}

impl ServerCapabilities {
    pub fn tools(&self) -> bool {
        self.tools
    }

    pub fn resources(&self) -> bool {
        self.resources
    }

    pub fn prompts(&self) -> bool {
        self.prompts
    }

    pub fn logging(&self) -> bool {
        self.logging
    }

    pub fn completions(&self) -> bool {
        self.completions
    }
}

impl McpSessionId {
    pub fn parse(value: &str) -> Result<Self, TransportError> {
        let value = parse_ident(value, MAX_MCP_SESSION_ID_BYTES)?;
        if value.chars().any(char::is_whitespace) {
            return Err(TransportError::InvalidIdent);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl NegotiatedHandshake {
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    pub fn client_capabilities(&self) -> &ClientCapabilities {
        &self.client_capabilities
    }

    pub fn server_capabilities(&self) -> &ServerCapabilities {
        &self.server_capabilities
    }

    pub fn server_info(&self) -> &ImplementationInfo {
        &self.server_info
    }

    pub fn session_id(&self) -> Option<&McpSessionId> {
        self.session_id.as_ref()
    }
}

impl IoBounds {
    pub fn new(max_frame_bytes: usize, timeout: Duration) -> Result<Self, TransportError> {
        if max_frame_bytes == 0 || max_frame_bytes > MAX_FRAME_BYTES {
            return Err(TransportError::InvalidBounds);
        }
        if timeout.is_zero() || timeout > MAX_IO_TIMEOUT {
            return Err(TransportError::InvalidBounds);
        }
        Ok(Self {
            max_frame_bytes,
            timeout,
        })
    }

    pub fn standard() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES,
            timeout: DEFAULT_IO_TIMEOUT,
        }
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl Default for IoBounds {
    fn default() -> Self {
        Self::standard()
    }
}

impl HttpAuthScope {
    /// Bind a handle to `mcp.http.auth/<scheme>/<host>/<port>`.
    pub fn new(handle: SecretRef, target: &CanonicalNetworkTarget) -> Result<Self, TransportError> {
        Ok(Self {
            handle,
            target: secret_target_for(target)?,
        })
    }

    pub fn handle(&self) -> &SecretRef {
        &self.handle
    }

    pub fn target(&self) -> &SecretTarget {
        &self.target
    }

    fn matches(&self, target: &CanonicalNetworkTarget) -> bool {
        match secret_target_for(target) {
            Ok(expected) => self.target == expected,
            Err(_) => false,
        }
    }
}

impl Debug for HttpAuthScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpAuthScope")
            .field("handle", &self.handle)
            .field("target", &self.target)
            .finish()
    }
}

impl HttpConnectRequest {
    pub fn new(
        url: impl Into<String>,
        capability: Capability,
        resource: ResourceDescriptor,
        credential: Option<HttpAuthScope>,
        bounds: IoBounds,
    ) -> Self {
        Self {
            url: url.into(),
            capability,
            resource,
            credential,
            bounds,
        }
    }
}

impl HttpRequest<'_> {
    pub fn target(&self) -> &CanonicalNetworkTarget {
        self.target
    }

    pub fn path(&self) -> &str {
        self.path
    }

    pub fn session_id(&self) -> Option<&McpSessionId> {
        self.session_id
    }

    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    pub fn credential(&self) -> Option<&SecretRef> {
        self.credential
    }

    pub fn body(&self) -> &[u8] {
        self.body
    }
}

impl HttpResponse {
    pub fn new(
        status: u16,
        session_id: Option<String>,
        location: Option<String>,
        body: Vec<u8>,
    ) -> Result<Self, TransportError> {
        if body.len() > MAX_EVENT_STREAM_BYTES {
            return Err(TransportError::FrameTooLarge);
        }
        Ok(Self {
            status,
            session_id,
            location,
            body,
            content_type: None,
            www_authenticate: None,
        })
    }

    /// Record the reply's `Content-Type`, which decides how the body is framed.
    pub fn with_content_type(mut self, content_type: Option<String>) -> Self {
        self.content_type = content_type;
        self
    }

    /// Record the reply's `WWW-Authenticate` header, untrusted server text.
    pub fn with_www_authenticate(mut self, header: Option<String>) -> Self {
        self.www_authenticate = header;
        self
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub fn www_authenticate(&self) -> Option<&str> {
        self.www_authenticate.as_deref()
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl<R: Read + Send + 'static, W: Write> StdioTransport<R, W> {
    /// Attach to pipes from a supervised child. This type does not spawn the
    /// child process itself, but it does spawn the background frame reader
    /// described on the type's own doc comment.
    pub fn from_pipes(reader: R, writer: W, job_id: Option<JobId>, bounds: IoBounds) -> Self {
        Self {
            frames: spawn_frame_reader(reader, bounds.max_frame_bytes),
            _reader: PhantomData,
            writer,
            job_id,
            bounds,
            closed: false,
        }
    }

    pub fn job_id(&self) -> Option<JobId> {
        self.job_id
    }
}

/// Runs [`read_newline_frame`] in a loop on a dedicated thread, sending each
/// parsed frame (or the terminal error) over the returned channel. Never
/// cancelled internally — cancellation and the I/O deadline are enforced by
/// `recv_frame`'s wait on the channel, not by this loop.
fn spawn_frame_reader<R: Read + Send + 'static>(
    mut reader: R,
    max_frame_bytes: usize,
) -> mpsc::Receiver<Result<Vec<u8>, TransportError>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let never_cancelled = CancellationToken::new();
        let mut leftover = Vec::new();
        loop {
            let frame = read_newline_frame(
                &mut reader,
                &mut leftover,
                max_frame_bytes,
                &never_cancelled,
            );
            let terminal = frame.is_err();
            if tx.send(frame).is_err() || terminal {
                return;
            }
        }
    });
    rx
}

impl<R: Read, W: Write> McpTransport for StdioTransport<R, W> {
    fn kind(&self) -> TransportKind {
        TransportKind::Stdio
    }

    fn send_frame(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), TransportError> {
        check_open(self.closed, cancel)?;
        check_frame_len(bytes, self.bounds.max_frame_bytes)?;
        write_all_cancellable(&mut self.writer, bytes, cancel)?;
        write_all_cancellable(&mut self.writer, b"\n", cancel)?;
        self.writer.flush().map_err(|_| TransportError::Io)
    }

    fn recv_frame(&mut self, cancel: &CancellationToken) -> Result<Vec<u8>, TransportError> {
        check_open(self.closed, cancel)?;
        let deadline = Instant::now() + self.bounds.timeout;
        loop {
            cancel_check(cancel)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TransportError::Timeout);
            }
            match self.frames.recv_timeout(remaining.min(RECV_POLL_INTERVAL)) {
                Ok(frame) => return frame,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(TransportError::Closed),
            }
        }
    }

    fn set_io_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.bounds = IoBounds::new(self.bounds.max_frame_bytes, timeout)?;
        Ok(())
    }

    fn io_timeout(&self) -> Option<Duration> {
        Some(self.bounds.timeout)
    }

    fn close(&mut self, cancel: &CancellationToken) -> Result<(), TransportError> {
        cancel_check(cancel)?;
        self.closed = true;
        Ok(())
    }
}

impl<I: StreamableHttpIo, Rsv: NetworkResolver> StreamableHttpTransport<I, Rsv> {
    /// Normalize, match `net.connect` scope, authorize egress, then store origin.
    pub fn connect(
        io: I,
        resolver: Rsv,
        proxy: EgressProxy,
        request: HttpConnectRequest,
        cancel: &CancellationToken,
    ) -> Result<Self, TransportError> {
        cancel_check(cancel)?;
        if proxy.client() != NetworkClient::Tool {
            return Err(TransportError::CapabilityDenied);
        }
        let path = extract_http_path(&request.url)?;
        let grant = authorize_http_origin(
            &request.url,
            request.capability,
            &request.resource,
            &proxy,
            &resolver,
            cancel,
        )?;
        let origin = grant.target().clone();
        if let Some(credential) = &request.credential
            && !credential.matches(&origin)
        {
            return Err(TransportError::CredentialScopeDenied);
        }
        let origin_url = origin_url(&origin);
        Ok(Self {
            io,
            resolver,
            proxy,
            origin_url,
            path,
            origin,
            capability: request.capability,
            resource: request.resource,
            credential: request.credential,
            bounds: request.bounds,
            session_id: None,
            closed: false,
            inbound: VecDeque::new(),
            challenge: None,
        })
    }

    pub fn origin(&self) -> &CanonicalNetworkTarget {
        &self.origin
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl<I, R> Debug for StreamableHttpTransport<I, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamableHttpTransport")
            .field("scheme", &self.origin.scheme().as_str())
            .field("host", &self.origin.host().as_canonical_str())
            .field("port", &self.origin.port())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl<I: StreamableHttpIo, Rsv: NetworkResolver> StreamableHttpTransport<I, Rsv> {
    /// One authorized POST; returns the JSON-RPC frames its reply carried.
    fn post(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<u8>>, TransportError> {
        check_open(self.closed, cancel)?;
        check_frame_len(bytes, self.bounds.max_frame_bytes)?;
        let mut hops = 0u8;
        let mut location: Option<String> = None;
        loop {
            cancel_check(cancel)?;
            let grant = if let Some(location) = location.as_deref() {
                hops = hops
                    .checked_add(1)
                    .filter(|n| *n <= MAX_HTTP_REDIRECTS)
                    .ok_or(TransportError::RedirectDenied)?;
                authorize_http_redirect(
                    &self.origin,
                    location,
                    self.capability,
                    &self.resource,
                    &self.proxy,
                    &self.resolver,
                    cancel,
                )?
            } else {
                authorize_http_origin(
                    &self.origin_url,
                    self.capability,
                    &self.resource,
                    &self.proxy,
                    &self.resolver,
                    cancel,
                )?
            };
            if let Some(credential) = &self.credential
                && !credential.matches(grant.target())
            {
                return Err(TransportError::CredentialScopeDenied);
            }
            let request = HttpRequest {
                target: grant.target(),
                path: &self.path,
                session_id: self.session_id.as_ref(),
                protocol_version: ProtocolVersion::TARGET,
                credential: self.credential.as_ref().map(HttpAuthScope::handle),
                body: bytes,
            };
            let response = self
                .io
                .exchange(&request, &grant, cancel, self.bounds.timeout)?;
            if (300..400).contains(&response.status) {
                let next = response
                    .location
                    .filter(|value| !value.is_empty())
                    .ok_or(TransportError::RedirectDenied)?;
                location = Some(next);
                continue;
            }
            match response.status {
                200 | 202 => {}
                401 => {
                    self.challenge = Some(parse_www_authenticate(
                        response.www_authenticate.as_deref().unwrap_or_default(),
                    ));
                    return Err(TransportError::Unauthorized);
                }
                403 => {
                    let challenge = parse_www_authenticate(
                        response.www_authenticate.as_deref().unwrap_or_default(),
                    );
                    let insufficient = challenge.error.as_deref() == Some("insufficient_scope");
                    self.challenge = Some(challenge);
                    return Err(if insufficient {
                        TransportError::InsufficientScope
                    } else {
                        TransportError::HandshakeFailed
                    });
                }
                _ => return Err(TransportError::HandshakeFailed),
            }
            self.challenge = None;
            if let Some(session_id) = response.session_id.as_deref() {
                self.session_id = Some(McpSessionId::parse(session_id)?);
            }
            let frames = split_reply_frames(
                response.content_type.as_deref(),
                &response.body,
                self.bounds.max_frame_bytes,
            )?;
            self.origin = grant.target().clone();
            self.origin_url = origin_url(&self.origin);
            return Ok(frames);
        }
    }
}

/// Whether an outbound frame is a request (has a method and an id), as opposed
/// to a notification or a response to a server request.
fn is_request_frame(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .is_some_and(|value| value.get("method").is_some() && value.get("id").is_some())
}

/// Turn one reply body into JSON-RPC frames: a JSON body is one frame, an
/// event stream is one frame per `message` event, an empty body (a `202`)
/// is none. A JSON frame is bounded by `max_frame_bytes`; a stream by
/// [`MAX_EVENT_STREAM_BYTES`] overall and `max_frame_bytes` per event.
fn split_reply_frames(
    content_type: Option<&str>,
    body: &[u8],
    max_frame_bytes: usize,
) -> Result<Vec<Vec<u8>>, TransportError> {
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let is_stream = content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
    });
    if !is_stream {
        if body.len() > max_frame_bytes {
            return Err(TransportError::FrameTooLarge);
        }
        return Ok(vec![body.to_vec()]);
    }
    if body.len() > MAX_EVENT_STREAM_BYTES {
        return Err(TransportError::FrameTooLarge);
    }
    let frames = parse_event_stream(body);
    if frames.iter().any(|frame| frame.len() > max_frame_bytes) {
        return Err(TransportError::FrameTooLarge);
    }
    Ok(frames)
}

/// The data of each `message` event in a `text/event-stream` body, in order.
/// Lines end in `\n`, `\r\n` or `\r`; `data` lines of one event are joined
/// with `\n`; comments, `id`, `retry` and events of any other type are
/// ignored. A final event without its terminating blank line is kept only if
/// its data is complete JSON, so a stream cut mid-event never yields a
/// fragment.
fn parse_event_stream(body: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut frames = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    let mut event_type: Option<&str> = None;
    let mut flush = |data: &mut Vec<&str>, event_type: &mut Option<&str>, terminated: bool| {
        let kind = event_type.take();
        if data.is_empty() {
            return;
        }
        let joined = data.join("\n");
        data.clear();
        if joined.is_empty() || kind.is_some_and(|kind| kind != "message") {
            return;
        }
        if !terminated && serde_json::from_str::<Value>(&joined).is_err() {
            return;
        }
        frames.push(joined.into_bytes());
    };
    let mut rest: &str = text;
    while !rest.is_empty() {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let line = &rest[..end];
        let mut skip = end;
        if rest[end..].starts_with("\r\n") {
            skip += 2;
        } else if end < rest.len() {
            skip += 1;
        }
        rest = &rest[skip..];
        if line.is_empty() {
            flush(&mut data, &mut event_type, true);
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => data.push(value),
            "event" => event_type = Some(value),
            _ => {}
        }
    }
    flush(&mut data, &mut event_type, false);
    frames
}

/// Parse the parameters of a `WWW-Authenticate` header. Total size is bounded
/// by [`MAX_CHALLENGE_BYTES`] (longer headers are dropped whole) and each
/// kept parameter by 1 KiB; control characters void a parameter.
pub fn parse_www_authenticate(header: &str) -> AuthChallenge {
    let mut challenge = AuthChallenge::default();
    if header.len() > MAX_CHALLENGE_BYTES {
        return challenge;
    }
    let bytes = header.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        // Skip separators and whitespace between tokens.
        while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b',') {
            index += 1;
        }
        let name_start = index;
        while index < bytes.len() && !matches!(bytes[index], b' ' | b'\t' | b',' | b'=') {
            index += 1;
        }
        let name = &header[name_start..index];
        // A token not followed by `=` is an auth scheme (`Bearer`), not a parameter.
        if index >= bytes.len() || bytes[index] != b'=' {
            continue;
        }
        index += 1;
        let value = if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' if index + 1 < bytes.len() => {
                        let escaped = header[index + 1..].chars().next().unwrap_or_default();
                        value.push(escaped);
                        index += 1 + escaped.len_utf8();
                    }
                    b'"' => {
                        index += 1;
                        closed = true;
                        break;
                    }
                    _ => {
                        let ch = header[index..].chars().next().unwrap_or_default();
                        value.push(ch);
                        index += ch.len_utf8();
                    }
                }
            }
            if !closed {
                break;
            }
            value
        } else {
            let start = index;
            while index < bytes.len() && !matches!(bytes[index], b' ' | b'\t' | b',') {
                index += 1;
            }
            header[start..index].to_owned()
        };
        if value.is_empty()
            || value.len() > MAX_CHALLENGE_PARAM_BYTES
            || value.chars().any(char::is_control)
        {
            continue;
        }
        let slot = match name.to_ascii_lowercase().as_str() {
            "resource_metadata" => &mut challenge.resource_metadata,
            "scope" => &mut challenge.scope,
            "error" => &mut challenge.error,
            _ => continue,
        };
        slot.get_or_insert(value);
    }
    challenge
}

impl<I: StreamableHttpIo, Rsv: NetworkResolver> McpTransport for StreamableHttpTransport<I, Rsv> {
    fn kind(&self) -> TransportKind {
        TransportKind::StreamableHttp
    }

    /// Streamable HTTP answers every POST within its own response: the
    /// id-matched reply is the body, or one event of the stream the body is.
    /// Other frames in the reply (progress notifications, server requests)
    /// are skipped.
    fn exchange_skipping_notifications(
        &mut self,
        request: &[u8],
        expected_id: u64,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, TransportError> {
        self.post(request, cancel)?
            .into_iter()
            .find(|frame| has_response_id(frame, expected_id))
            .ok_or(TransportError::InvalidFrame)
    }

    /// POST one frame and queue whatever the reply carried for
    /// [`Self::recv_frame`]. A request starts a fresh exchange (anything
    /// unread from an earlier one is dropped); a notification or a response
    /// to a server request adds to the queue, whose frames the caller is
    /// still reading.
    fn send_frame(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), TransportError> {
        if is_request_frame(bytes) {
            self.inbound.clear();
        }
        let frames = self.post(bytes, cancel)?;
        self.inbound.extend(frames);
        Ok(())
    }

    fn recv_frame(&mut self, cancel: &CancellationToken) -> Result<Vec<u8>, TransportError> {
        check_open(self.closed, cancel)?;
        self.inbound.pop_front().ok_or(TransportError::InvalidFrame)
    }

    /// One request and its single reply: the first frame that is a response
    /// (no `method`), or, failing that, the first frame.
    fn call(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, TransportError> {
        let mut frames = self.post(bytes, cancel)?;
        if frames.is_empty() {
            return Ok(Vec::new());
        }
        let index = frames
            .iter()
            .position(|frame| {
                serde_json::from_slice::<Value>(frame)
                    .is_ok_and(|value| value.get("method").is_none())
            })
            .unwrap_or(0);
        Ok(frames.swap_remove(index))
    }

    fn session_id(&self) -> Option<&McpSessionId> {
        self.session_id.as_ref()
    }

    fn auth_challenge(&self) -> Option<&AuthChallenge> {
        self.challenge.as_ref()
    }

    fn set_io_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.bounds = IoBounds::new(self.bounds.max_frame_bytes, timeout)?;
        Ok(())
    }

    fn io_timeout(&self) -> Option<Duration> {
        Some(self.bounds.timeout)
    }

    fn close(&mut self, cancel: &CancellationToken) -> Result<(), TransportError> {
        cancel_check(cancel)?;
        self.closed = true;
        Ok(())
    }
}

impl LoopbackTransport {
    pub fn new(kind: TransportKind, bounds: IoBounds) -> Self {
        Self {
            kind,
            inbound: VecDeque::new(),
            outbound: Vec::new(),
            session_id: None,
            bounds,
            closed: false,
        }
    }

    pub fn push_inbound(&mut self, frame: impl Into<Vec<u8>>) -> Result<(), TransportError> {
        let frame = frame.into();
        check_frame_len(&frame, self.bounds.max_frame_bytes)?;
        self.inbound.push_back(frame);
        Ok(())
    }

    pub fn outbound(&self) -> &[Vec<u8>] {
        &self.outbound
    }

    pub fn set_session_id(&mut self, session_id: McpSessionId) {
        self.session_id = Some(session_id);
    }
}

impl McpTransport for LoopbackTransport {
    fn kind(&self) -> TransportKind {
        self.kind
    }

    fn send_frame(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), TransportError> {
        check_open(self.closed, cancel)?;
        check_frame_len(bytes, self.bounds.max_frame_bytes)?;
        self.outbound.push(bytes.to_vec());
        Ok(())
    }

    fn recv_frame(&mut self, cancel: &CancellationToken) -> Result<Vec<u8>, TransportError> {
        check_open(self.closed, cancel)?;
        self.inbound.pop_front().ok_or(TransportError::Closed)
    }

    fn session_id(&self) -> Option<&McpSessionId> {
        self.session_id.as_ref()
    }

    fn set_io_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.bounds = IoBounds::new(self.bounds.max_frame_bytes, timeout)?;
        Ok(())
    }

    fn io_timeout(&self) -> Option<Duration> {
        Some(self.bounds.timeout)
    }

    fn close(&mut self, cancel: &CancellationToken) -> Result<(), TransportError> {
        cancel_check(cancel)?;
        self.closed = true;
        Ok(())
    }
}

impl<T: McpTransport> McpSession<T> {
    pub fn new(
        transport: T,
        client_info: ImplementationInfo,
        client_capabilities: ClientCapabilities,
    ) -> Self {
        Self {
            transport,
            client_info,
            client_capabilities,
            next_id: 1,
            handshake: None,
            closed: false,
        }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn handshake(&self) -> Option<&NegotiatedHandshake> {
        self.handshake.as_ref()
    }

    /// Offer `2026-07-28` and record the negotiated supported version/capabilities.
    pub fn initialize(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<&NegotiatedHandshake, TransportError> {
        check_open(self.closed, cancel)?;
        if self.handshake.is_some() {
            return Err(TransportError::AlreadyInitialized);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = encode_initialize(id, &self.client_info, &self.client_capabilities)?;
        let response = self.transport.call(&request, cancel)?;
        let parsed = match parse_initialize_result(&response, id) {
            Ok(parsed) => parsed,
            Err(err) => {
                let _ = self.close(cancel);
                return Err(err);
            }
        };
        let handshake = NegotiatedHandshake {
            protocol_version: parsed.version,
            client_capabilities: self.client_capabilities.clone(),
            server_capabilities: parsed.capabilities,
            server_info: parsed.server_info,
            session_id: self.transport.session_id().cloned(),
        };
        let initialized = encode_initialized()?;
        self.transport.send_frame(&initialized, cancel)?;
        self.handshake = Some(handshake);
        self.handshake
            .as_ref()
            .ok_or(TransportError::HandshakeFailed)
    }

    /// List the tools the server advertises (`tools/list`).
    pub fn tools_list(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Vec<McpToolDescriptor>, TransportError> {
        check_open(self.closed, cancel)?;
        if self.handshake.is_none() {
            return Err(TransportError::NotInitialized);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = encode_tools_list(id)?;
        // Skip server->client notification frames (no id) before the reply;
        // the transport itself decides what "exchange" means (newline
        // framing for stdio, one authorized POST for streamable HTTP).
        let response = self
            .transport
            .exchange_skipping_notifications(&request, id, cancel)?;
        parse_tools_list_result(&response, id)
    }

    /// Invoke one server tool (`tools/call`); returns its text content and
    /// error flag. A JSON-RPC error response maps to [`TransportError::ToolFailed`].
    pub fn tools_call(
        &mut self,
        name: &str,
        arguments: &Value,
        cancel: &CancellationToken,
    ) -> Result<McpToolCallOutput, TransportError> {
        check_open(self.closed, cancel)?;
        if self.handshake.is_none() {
            return Err(TransportError::NotInitialized);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = encode_tools_call(id, name, arguments)?;
        let response = self
            .transport
            .exchange_skipping_notifications(&request, id, cancel)?;
        parse_tools_call_result(&response, id)
    }

    /// [`Self::tools_call`], answering each request the server makes while
    /// the call runs (`elicitation/create`, …) with `on_request(method,
    /// params)`: `Ok(result)` is sent as the request's result, `Err(message)`
    /// as a JSON-RPC error. A server request is never left unanswered — the
    /// server would wait on it forever.
    pub fn tools_call_with(
        &mut self,
        name: &str,
        arguments: &Value,
        cancel: &CancellationToken,
        on_request: &mut dyn FnMut(&str, &Value) -> Result<Value, String>,
    ) -> Result<McpToolCallOutput, TransportError> {
        check_open(self.closed, cancel)?;
        if self.handshake.is_none() {
            return Err(TransportError::NotInitialized);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = encode_tools_call(id, name, arguments)?;
        let result = self.serve_tools_call(id, &request, cancel, on_request);
        // A call given up on (it ran out of time, or the caller stopped it)
        // is told so: the server can stop working on it. Best effort, and
        // never allowed to take long.
        if let Err(reason @ (TransportError::Timeout | TransportError::Cancelled)) = &result {
            self.send_cancelled(id, reason);
        }
        result
    }

    /// Change the deadline of each later send or receive (a call may be
    /// given longer than the handshake was).
    pub fn set_io_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.transport.set_io_timeout(timeout)
    }

    /// `notifications/cancelled` for request `id`, sent on a fresh token (the
    /// call's own is the reason for sending it) within a short deadline. A
    /// failure to send it is of no further interest.
    fn send_cancelled(&mut self, id: u64, why: &TransportError) {
        let reason = match why {
            TransportError::Timeout => "the client gave up waiting",
            _ => "the client cancelled the call",
        };
        let mut params = Map::new();
        params.insert("requestId".to_owned(), Value::from(id));
        params.insert("reason".to_owned(), Value::String(reason.to_owned()));
        let mut body = Map::new();
        body.insert(
            "jsonrpc".to_owned(),
            Value::String(JSONRPC_VERSION.to_owned()),
        );
        body.insert(
            "method".to_owned(),
            Value::String(CANCELLED_METHOD.to_owned()),
        );
        body.insert("params".to_owned(), Value::Object(params));
        let Ok(bytes) = encode_json(Value::Object(body)) else {
            return;
        };
        // The notice has a short deadline of its own; the call's is put back
        // for whatever the session does next.
        let previous = self.transport.io_timeout();
        let _ = self.transport.set_io_timeout(CANCEL_NOTICE_TIMEOUT);
        let _ = self.transport.send_frame(&bytes, &CancellationToken::new());
        if let Some(previous) = previous {
            let _ = self.transport.set_io_timeout(previous);
        }
    }

    fn serve_tools_call(
        &mut self,
        id: u64,
        request: &[u8],
        cancel: &CancellationToken,
        on_request: &mut dyn FnMut(&str, &Value) -> Result<Value, String>,
    ) -> Result<McpToolCallOutput, TransportError> {
        self.transport.send_frame(request, cancel)?;
        let mut served = 0usize;
        loop {
            let frame = self.transport.recv_frame(cancel)?;
            if has_response_id(&frame, id) {
                return parse_tools_call_result(&frame, id);
            }
            let Ok(value) = serde_json::from_slice::<Value>(&frame) else {
                continue;
            };
            // A request from the server: it has a method and an id.
            let (Some(method), Some(request_id)) =
                (value.get("method").and_then(Value::as_str), value.get("id"))
            else {
                continue;
            };
            // A server that keeps asking during one call is not answered
            // forever: past the bound the call fails.
            served += 1;
            if served > MAX_SERVER_REQUESTS_PER_CALL {
                // What the server sends next is out of step with any later
                // call: the session is done.
                self.closed = true;
                return Err(TransportError::ToolFailed);
            }
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let reply = match on_request(method, &params) {
                Ok(result) => serde_json::json!({
                    "jsonrpc": JSONRPC_VERSION, "id": request_id, "result": result,
                }),
                Err(message) => serde_json::json!({
                    "jsonrpc": JSONRPC_VERSION, "id": request_id,
                    "error": {"code": -32601, "message": message},
                }),
            };
            let bytes = serde_json::to_vec(&reply).map_err(|_| TransportError::InvalidFrame)?;
            self.transport.send_frame(&bytes, cancel)?;
        }
    }

    pub fn close(&mut self, cancel: &CancellationToken) -> Result<(), TransportError> {
        self.closed = true;
        self.transport.close(cancel)
    }
}

struct ParsedInitialize {
    version: ProtocolVersion,
    capabilities: ServerCapabilities,
    server_info: ImplementationInfo,
}

fn encode_initialize(
    id: u64,
    client_info: &ImplementationInfo,
    capabilities: &ClientCapabilities,
) -> Result<Vec<u8>, TransportError> {
    let mut caps = Map::new();
    if capabilities.roots {
        caps.insert("roots".to_owned(), Value::Object(Map::new()));
    }
    if capabilities.elicitation {
        caps.insert("elicitation".to_owned(), Value::Object(Map::new()));
    }
    let mut info = Map::new();
    info.insert("name".to_owned(), Value::String(client_info.name.clone()));
    info.insert(
        "version".to_owned(),
        Value::String(client_info.version.clone()),
    );
    let mut params = Map::new();
    params.insert(
        "protocolVersion".to_owned(),
        Value::String(ProtocolVersion::TARGET.as_str().to_owned()),
    );
    params.insert("capabilities".to_owned(), Value::Object(caps));
    params.insert("clientInfo".to_owned(), Value::Object(info));
    let mut body = Map::new();
    body.insert(
        "jsonrpc".to_owned(),
        Value::String(JSONRPC_VERSION.to_owned()),
    );
    body.insert("id".to_owned(), Value::from(id));
    body.insert(
        "method".to_owned(),
        Value::String(INITIALIZE_METHOD.to_owned()),
    );
    body.insert("params".to_owned(), Value::Object(params));
    encode_json(Value::Object(body))
}

fn encode_initialized() -> Result<Vec<u8>, TransportError> {
    let mut body = Map::new();
    body.insert(
        "jsonrpc".to_owned(),
        Value::String(JSONRPC_VERSION.to_owned()),
    );
    body.insert(
        "method".to_owned(),
        Value::String(INITIALIZED_METHOD.to_owned()),
    );
    body.insert("params".to_owned(), Value::Object(Map::new()));
    encode_json(Value::Object(body))
}

const TOOLS_LIST_METHOD: &str = "tools/list";
const TOOLS_CALL_METHOD: &str = "tools/call";
const CANCELLED_METHOD: &str = "notifications/cancelled";

fn encode_tools_list(id: u64) -> Result<Vec<u8>, TransportError> {
    let mut body = Map::new();
    body.insert(
        "jsonrpc".to_owned(),
        Value::String(JSONRPC_VERSION.to_owned()),
    );
    body.insert("id".to_owned(), Value::from(id));
    body.insert(
        "method".to_owned(),
        Value::String(TOOLS_LIST_METHOD.to_owned()),
    );
    body.insert("params".to_owned(), Value::Object(Map::new()));
    encode_json(Value::Object(body))
}

fn encode_tools_call(id: u64, name: &str, arguments: &Value) -> Result<Vec<u8>, TransportError> {
    let mut params = Map::new();
    params.insert("name".to_owned(), Value::String(name.to_owned()));
    params.insert("arguments".to_owned(), arguments.clone());
    let mut body = Map::new();
    body.insert(
        "jsonrpc".to_owned(),
        Value::String(JSONRPC_VERSION.to_owned()),
    );
    body.insert("id".to_owned(), Value::from(id));
    body.insert(
        "method".to_owned(),
        Value::String(TOOLS_CALL_METHOD.to_owned()),
    );
    body.insert("params".to_owned(), Value::Object(params));
    encode_json(Value::Object(body))
}

/// Whether a raw JSON-RPC frame is the reply to `expected_id` (notifications
/// carry no id and are skipped by the caller).
fn has_response_id(frame: &[u8], expected_id: u64) -> bool {
    match serde_json::from_slice::<Value>(frame) {
        // A server *request* carries an id too (its own); only a frame
        // without a method is a response.
        Ok(value) if value.get("method").is_some() => false,
        Ok(value) => match value.get("id") {
            Some(Value::Number(n)) => n.as_u64() == Some(expected_id),
            Some(Value::String(s)) => s.parse::<u64>().ok() == Some(expected_id),
            _ => false,
        },
        Err(_) => false,
    }
}

/// The most requests a server may make of the client during one call.
pub const MAX_SERVER_REQUESTS_PER_CALL: usize = 16;

/// Shared JSON-RPC response checks: version, id match, error mapping.
fn parse_rpc_response(
    bytes: &[u8],
    expected_id: u64,
    error_variant: TransportError,
) -> Result<Value, TransportError> {
    let value = parse_json_object(bytes)?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(TransportError::InvalidFrame);
    }
    if value.get("error").is_some() {
        return Err(error_variant);
    }
    let id_ok = match value.get("id") {
        Some(Value::Number(n)) => n.as_u64() == Some(expected_id),
        Some(Value::String(s)) => s.parse::<u64>().ok() == Some(expected_id),
        _ => false,
    };
    if !id_ok {
        return Err(TransportError::InvalidFrame);
    }
    value
        .get("result")
        .and_then(Value::as_object)
        .map(|object| Value::Object(object.clone()))
        .ok_or(error_variant)
}

fn parse_tools_list_result(
    bytes: &[u8],
    expected_id: u64,
) -> Result<Vec<McpToolDescriptor>, TransportError> {
    let result = parse_rpc_response(bytes, expected_id, TransportError::InvalidFrame)?;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or(TransportError::InvalidFrame)?;
    let mut descriptors = Vec::new();
    for tool in tools {
        let Some(object) = tool.as_object() else {
            continue;
        };
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        descriptors.push(McpToolDescriptor {
            name: name.to_owned(),
            description: object
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            input_schema: object
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
        });
    }
    Ok(descriptors)
}

fn parse_tools_call_result(
    bytes: &[u8],
    expected_id: u64,
) -> Result<McpToolCallOutput, TransportError> {
    let result = parse_rpc_response(bytes, expected_id, TransportError::ToolFailed)?;
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut text = String::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for part in content {
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(piece) = part.get("text").and_then(Value::as_str)
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(piece);
            }
        }
    }
    Ok(McpToolCallOutput { text, is_error })
}

fn encode_json(value: Value) -> Result<Vec<u8>, TransportError> {
    let bytes = serde_json::to_vec(&value).map_err(|_| TransportError::Io)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge);
    }
    Ok(bytes)
}

fn parse_initialize_result(
    bytes: &[u8],
    expected_id: u64,
) -> Result<ParsedInitialize, TransportError> {
    let value = parse_json_object(bytes)?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(TransportError::InvalidFrame);
    }
    if value.get("error").is_some() {
        return Err(TransportError::HandshakeFailed);
    }
    let id_ok = match value.get("id") {
        Some(Value::Number(n)) => n.as_u64() == Some(expected_id),
        Some(Value::String(s)) => s.parse::<u64>().ok() == Some(expected_id),
        _ => false,
    };
    if !id_ok {
        return Err(TransportError::InvalidFrame);
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or(TransportError::HandshakeFailed)?;
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or(TransportError::HandshakeFailed)?;
    let version = ProtocolVersion::parse(version)?;
    let capabilities = parse_server_capabilities(result.get("capabilities"));
    let server_info = parse_server_info(result.get("serverInfo"))?;
    Ok(ParsedInitialize {
        version,
        capabilities,
        server_info,
    })
}

fn parse_server_capabilities(value: Option<&Value>) -> ServerCapabilities {
    let Some(object) = value.and_then(Value::as_object) else {
        return ServerCapabilities::default();
    };
    ServerCapabilities {
        tools: object.contains_key("tools"),
        resources: object.contains_key("resources"),
        prompts: object.contains_key("prompts"),
        logging: object.contains_key("logging"),
        completions: object.contains_key("completions"),
    }
}

fn parse_server_info(value: Option<&Value>) -> Result<ImplementationInfo, TransportError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or(TransportError::HandshakeFailed)?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or(TransportError::HandshakeFailed)?;
    let version = object.get("version").and_then(Value::as_str).unwrap_or("0");
    ImplementationInfo::new(name, version)
}

fn parse_json_object(bytes: &[u8]) -> Result<Map<String, Value>, TransportError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| TransportError::InvalidFrame)?;
    let value: Value = serde_json::from_str(text).map_err(|_| TransportError::InvalidFrame)?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(TransportError::InvalidFrame),
    }
}

fn authorize_http_origin<Rsv: NetworkResolver + ?Sized>(
    url: &str,
    capability: Capability,
    resource: &ResourceDescriptor,
    proxy: &EgressProxy,
    resolver: &Rsv,
    cancel: &CancellationToken,
) -> Result<ConsumedConnect, TransportError> {
    cancel_check(cancel)?;
    if capability != Capability::NetConnect {
        return Err(TransportError::CapabilityDenied);
    }
    let intent = NetworkIntent::connect(url);
    let target = normalize_network(&intent, resolver, cancel).map_err(map_normalize)?;
    match_network_scope(resource, &target)?;
    let grant = consume_http_grant(proxy, &intent, resolver, cancel)?;
    if grant.target() != &target && !same_origin(grant.target(), &target) {
        return Err(TransportError::NetworkDenied);
    }
    Ok(grant)
}

fn authorize_http_redirect<Rsv: NetworkResolver + ?Sized>(
    previous: &CanonicalNetworkTarget,
    location: &str,
    capability: Capability,
    resource: &ResourceDescriptor,
    proxy: &EgressProxy,
    resolver: &Rsv,
    cancel: &CancellationToken,
) -> Result<ConsumedConnect, TransportError> {
    cancel_check(cancel)?;
    if capability != Capability::NetConnect {
        return Err(TransportError::CapabilityDenied);
    }
    let intent = NetworkIntent::redirect(previous, location).map_err(map_normalize)?;
    let target = normalize_network(&intent, resolver, cancel).map_err(map_normalize)?;
    match_network_scope(resource, &target)?;
    consume_http_grant(proxy, &intent, resolver, cancel)
}

fn consume_http_grant<Rsv: NetworkResolver + ?Sized>(
    proxy: &EgressProxy,
    intent: &NetworkIntent,
    resolver: &Rsv,
    cancel: &CancellationToken,
) -> Result<ConsumedConnect, TransportError> {
    let lease = match authorize_connect(proxy, intent, resolver, cancel).map_err(map_egress)? {
        EgressOutcome::Allow(lease) => lease,
        EgressOutcome::Deny(_) => return Err(TransportError::NetworkDenied),
    };
    match proxy
        .consume_connect(&lease, None, resolver, cancel)
        .map_err(map_egress)?
    {
        EgressOutcome::Allow(consumed) => Ok(consumed),
        EgressOutcome::Deny(_) => Err(TransportError::NetworkDenied),
    }
}

fn match_network_scope(
    resource: &ResourceDescriptor,
    target: &CanonicalNetworkTarget,
) -> Result<(), TransportError> {
    let ResourceDescriptor::Network(scope) = resource else {
        return Err(TransportError::NetworkScopeDenied);
    };
    if !network_scope_matches(scope, target) {
        return Err(TransportError::NetworkScopeDenied);
    }
    Ok(())
}

fn network_scope_matches(scope: &NetworkScope, target: &CanonicalNetworkTarget) -> bool {
    if scope.scheme() != target.scheme() || scope.port() != target.port() {
        return false;
    }
    match target.host() {
        CanonicalNetHost::Dns(host) => scope.host().as_str() == host.as_str(),
        CanonicalNetHost::Ip(ip) => scope.host().as_str() == ip.to_string(),
    }
}

fn extract_http_path(url: &str) -> Result<String, TransportError> {
    if url.len() > MAX_URL_BYTES {
        return Err(TransportError::InvalidFrame);
    }
    if url.contains('\0') || url.chars().any(char::is_control) {
        return Err(TransportError::InvalidFrame);
    }
    if url.contains('@') {
        return Err(TransportError::NetworkDenied);
    }
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or(TransportError::InvalidFrame)?;
    let path = match rest.find(['/', '?', '#']) {
        Some(idx) => &rest[idx..],
        None => "/",
    };
    if path.len() > MAX_HTTP_PATH_BYTES {
        return Err(TransportError::FrameTooLarge);
    }
    Ok(path.to_owned())
}

fn origin_url(target: &CanonicalNetworkTarget) -> String {
    format!(
        "{}://{}:{}",
        target.scheme().as_str(),
        format_host(target.host()),
        target.port()
    )
}

fn format_host(host: &CanonicalNetHost) -> String {
    match host {
        CanonicalNetHost::Dns(name) => name.as_str().to_owned(),
        CanonicalNetHost::Ip(std::net::IpAddr::V6(v6)) => format!("[{v6}]"),
        CanonicalNetHost::Ip(std::net::IpAddr::V4(v4)) => v4.to_string(),
    }
}

fn same_origin(left: &CanonicalNetworkTarget, right: &CanonicalNetworkTarget) -> bool {
    left.scheme() == right.scheme()
        && left.port() == right.port()
        && left.host().as_canonical_str() == right.host().as_canonical_str()
}

fn secret_target_for(target: &CanonicalNetworkTarget) -> Result<SecretTarget, TransportError> {
    let domain = format!(
        "{SECRET_TARGET_PREFIX}/{}/{}/{}",
        target.scheme().as_str(),
        target.host().as_canonical_str(),
        target.port()
    );
    SecretTarget::new(&domain).map_err(|_| TransportError::CredentialScopeDenied)
}

fn parse_ident(value: &str, max: usize) -> Result<String, TransportError> {
    if value.is_empty() {
        return Err(TransportError::InvalidIdent);
    }
    if value.len() > max {
        return Err(TransportError::InvalidIdent);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(TransportError::InvalidIdent);
    }
    Ok(value.to_owned())
}

fn check_frame_len(bytes: &[u8], max: usize) -> Result<(), TransportError> {
    if bytes.is_empty() {
        return Err(TransportError::InvalidFrame);
    }
    if bytes.len() > max {
        return Err(TransportError::FrameTooLarge);
    }
    Ok(())
}

fn check_open(closed: bool, cancel: &CancellationToken) -> Result<(), TransportError> {
    cancel_check(cancel)?;
    if closed {
        Err(TransportError::Closed)
    } else {
        Ok(())
    }
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), TransportError> {
    if cancel.is_cancelled() {
        Err(TransportError::Cancelled)
    } else {
        Ok(())
    }
}

fn write_all_cancellable<W: Write>(
    writer: &mut W,
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<(), TransportError> {
    let mut offset = 0;
    while offset < bytes.len() {
        cancel_check(cancel)?;
        match writer.write(&bytes[offset..]) {
            Ok(0) => return Err(TransportError::Io),
            Ok(n) => offset += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                return Err(TransportError::Timeout);
            }
            Err(_) => return Err(TransportError::Io),
        }
    }
    Ok(())
}

fn read_newline_frame<R: Read>(
    reader: &mut R,
    leftover: &mut Vec<u8>,
    max: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, TransportError> {
    let mut tmp = [0u8; 512];
    loop {
        cancel_check(cancel)?;
        if let Some(idx) = leftover.iter().position(|b| *b == b'\n') {
            let mut frame: Vec<u8> = leftover.drain(..=idx).collect();
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            if frame.len() > max {
                leftover.clear();
                return Err(TransportError::FrameTooLarge);
            }
            if frame.is_empty() {
                return Err(TransportError::InvalidFrame);
            }
            return Ok(frame);
        }
        if leftover.len() > max {
            leftover.clear();
            return Err(TransportError::FrameTooLarge);
        }
        match reader.read(&mut tmp) {
            Ok(0) => return Err(TransportError::Closed),
            Ok(n) => {
                leftover.extend_from_slice(&tmp[..n]);
                if leftover.len() > max {
                    leftover.clear();
                    return Err(TransportError::FrameTooLarge);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                return Err(TransportError::Timeout);
            }
            Err(_) => return Err(TransportError::Io),
        }
        if leftover.len().is_multiple_of(CANCEL_STRIDE) {
            cancel_check(cancel)?;
        }
    }
}

fn map_normalize(err: NetworkNormalizeError) -> TransportError {
    match err {
        NetworkNormalizeError::Cancelled => TransportError::Cancelled,
        NetworkNormalizeError::Userinfo => TransportError::NetworkDenied,
        NetworkNormalizeError::TooLong
        | NetworkNormalizeError::TooManyIps
        | NetworkNormalizeError::TooManyRedirects => TransportError::FrameTooLarge,
        _ => TransportError::InvalidFrame,
    }
}

fn map_egress(err: EgressError) -> TransportError {
    match err {
        EgressError::Cancelled => TransportError::Cancelled,
        EgressError::Normalize(inner) => map_normalize(inner),
        _ => TransportError::NetworkDenied,
    }
}

impl TransportError {
    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::NetworkDenied
            | Self::NetworkScopeDenied
            | Self::CredentialScopeDenied
            | Self::CapabilityDenied
            | Self::RedirectDenied => Some(ErrorCode::PolicyDenied),
            Self::Timeout => Some(ErrorCode::ProcessTimeout),
            Self::FrameTooLarge
            | Self::InvalidFrame
            | Self::InvalidBounds
            | Self::InvalidIdent
            | Self::UnsupportedProtocolVersion
            | Self::HandshakeFailed
            | Self::NotInitialized
            | Self::AlreadyInitialized
            | Self::Closed
            | Self::ToolFailed => Some(ErrorCode::ToolInvalidArguments),
            Self::Io => Some(ErrorCode::InternalUnexpected),
            Self::Unauthorized | Self::InsufficientScope => Some(ErrorCode::AuthRequired),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::Timeout => "MCP transport timed out",
            Self::FrameTooLarge => "MCP frame exceeds the configured bound",
            Self::InvalidFrame => "MCP frame is not valid JSON-RPC",
            Self::InvalidBounds => "MCP I/O bounds are invalid",
            Self::InvalidIdent => "MCP identifier exceeds the allowed charset or bound",
            Self::UnsupportedProtocolVersion => "MCP protocol version is not supported",
            Self::HandshakeFailed => "MCP initialize handshake failed",
            Self::NetworkDenied => "MCP remote HTTP was denied by network policy",
            Self::NetworkScopeDenied => "MCP remote HTTP is outside the net.connect scope",
            Self::CredentialScopeDenied => "MCP HTTP credential scope does not match the origin",
            Self::CapabilityDenied => "MCP remote HTTP requires net.connect",
            Self::NotInitialized => "MCP session is not initialized",
            Self::AlreadyInitialized => "MCP session is already initialized",
            Self::ToolFailed => "MCP tool call failed on the server",
            Self::Closed => "MCP transport is closed",
            Self::RedirectDenied => "MCP HTTP redirect was denied",
            Self::Io => UNKNOWN_INTERNAL_MESSAGE,
            Self::Unauthorized => "MCP server requires authorization",
            Self::InsufficientScope => "MCP server needs a broader authorization scope",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable-http",
        })
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "MCP transport cancelled",
            Self::Timeout => "MCP transport timed out",
            Self::FrameTooLarge => "MCP frame exceeds the configured bound",
            Self::InvalidFrame => "MCP frame is not valid JSON-RPC",
            Self::InvalidBounds => "MCP I/O bounds are invalid",
            Self::InvalidIdent => "MCP identifier is invalid",
            Self::UnsupportedProtocolVersion => "MCP protocol version is not supported",
            Self::HandshakeFailed => "MCP initialize handshake failed",
            Self::NetworkDenied => "MCP remote HTTP denied by network policy",
            Self::NetworkScopeDenied => "MCP remote HTTP outside net.connect scope",
            Self::CredentialScopeDenied => "MCP HTTP credential scope mismatch",
            Self::CapabilityDenied => "MCP remote HTTP requires net.connect",
            Self::NotInitialized => "MCP session is not initialized",
            Self::AlreadyInitialized => "MCP session is already initialized",
            Self::ToolFailed => "MCP tool call failed on the server",
            Self::Closed => "MCP transport is closed",
            Self::RedirectDenied => "MCP HTTP redirect denied",
            Self::Io => "MCP transport I/O failed",
            Self::Unauthorized => "MCP server requires authorization (HTTP 401)",
            Self::InsufficientScope => "MCP server needs a broader scope (HTTP 403)",
        })
    }
}

impl Error for TransportError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::net::IpAddr;
    use std::sync::{Arc, Mutex};

    use capability_broker::{Hostname, NetworkNormalizeError, NetworkScheme};
    use security::{EgressPolicy, EgressRule};

    struct MapResolver {
        records: BTreeMap<String, Vec<IpAddr>>,
    }

    impl MapResolver {
        fn fixture() -> Self {
            let mut records = BTreeMap::new();
            records.insert(
                "mcp.example.com".to_owned(),
                vec!["93.184.216.34".parse().expect("ip")],
            );
            records.insert(
                "evil.example.com".to_owned(),
                vec!["198.51.100.10".parse().expect("ip")],
            );
            records.insert(
                "localhost".to_owned(),
                vec!["127.0.0.1".parse().expect("ip")],
            );
            records.insert(
                "metadata.google.internal".to_owned(),
                vec!["169.254.169.254".parse().expect("ip")],
            );
            Self { records }
        }
    }

    impl NetworkResolver for MapResolver {
        fn resolve(&self, host: &Hostname) -> Result<Vec<IpAddr>, NetworkNormalizeError> {
            self.records
                .get(host.as_str())
                .cloned()
                .ok_or(NetworkNormalizeError::UnresolvedHost)
        }
    }

    #[derive(Clone, Default)]
    struct RecordedHttp {
        posts: Arc<Mutex<Vec<RecordedPost>>>,
        script: Arc<Mutex<VecDeque<HttpResponse>>>,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedPost {
        host: String,
        path: String,
        used_secret: bool,
        body_len: usize,
    }

    impl RecordedHttp {
        fn push(&self, response: HttpResponse) {
            self.script.lock().expect("script").push_back(response);
        }

        fn posts(&self) -> Vec<RecordedPost> {
            self.posts.lock().expect("posts").clone()
        }
    }

    impl StreamableHttpIo for RecordedHttp {
        fn exchange(
            &mut self,
            request: &HttpRequest<'_>,
            grant: &ConsumedConnect,
            cancel: &CancellationToken,
            _timeout: Duration,
        ) -> Result<HttpResponse, TransportError> {
            if cancel.is_cancelled() {
                return Err(TransportError::Cancelled);
            }
            assert_eq!(
                grant.target().host().as_canonical_str(),
                request.target().host().as_canonical_str()
            );
            self.posts.lock().expect("posts").push(RecordedPost {
                host: request.target().host().as_canonical_str(),
                path: request.path().to_owned(),
                used_secret: request.credential().is_some(),
                body_len: request.body().len(),
            });
            self.script
                .lock()
                .expect("script")
                .pop_front()
                .ok_or(TransportError::Closed)
        }
    }

    fn initialize_ok(version: &str) -> Vec<u8> {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}},"resources":{{}}}},"serverInfo":{{"name":"fixture","version":"1.0"}}}}}}"#
        )
        .into_bytes()
    }

    fn net_resource(host: &str, port: u16) -> ResourceDescriptor {
        ResourceDescriptor::Network(
            NetworkScope::new(NetworkScheme::Https, host, port).expect("scope"),
        )
    }

    fn tool_proxy(hosts: &[&str]) -> EgressProxy {
        let rules = hosts
            .iter()
            .map(|host| EgressRule::host(host).expect("rule"))
            .collect::<Vec<_>>();
        EgressProxy::new(
            EgressPolicy::allowlist(rules).expect("policy"),
            NetworkClient::Tool,
        )
    }

    fn connect_http(
        io: RecordedHttp,
        url: &str,
        host: &str,
        credential: Option<HttpAuthScope>,
    ) -> Result<StreamableHttpTransport<RecordedHttp, MapResolver>, TransportError> {
        StreamableHttpTransport::connect(
            io,
            MapResolver::fixture(),
            tool_proxy(&[host]),
            HttpConnectRequest::new(
                url,
                Capability::NetConnect,
                net_resource(host, 443),
                credential,
                IoBounds::standard(),
            ),
            &CancellationToken::new(),
        )
    }

    #[test]
    fn tools_list_parses_advertised_descriptors() {
        let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        transport
            .push_inbound(initialize_ok(MCP_PROTOCOL_VERSION))
            .expect("inbound");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::new(true),
        );
        session.initialize(&CancellationToken::new()).expect("init");
        session
            .transport_mut()
            .push_inbound(
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[
                    {"name":"echo","description":"echoes input","inputSchema":{"type":"object"}},
                    {"name":"noname"},
                    {"name":"calc","inputSchema":{"type":"object"}}
                ]}}"#
                    .as_bytes()
                    .to_vec(),
            )
            .expect("inbound");
        let tools = session.tools_list(&CancellationToken::new()).expect("list");
        assert_eq!(tools.len(), 3, "nameless entry skipped, named kept");
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("echoes input"));
        assert_eq!(
            tools[1].name, "noname",
            "schema-less entry keeps default schema"
        );
        assert_eq!(tools[1].input_schema, serde_json::json!({}));
        assert_eq!(tools[2].name, "calc");
        assert_eq!(tools[2].description, None);
        let request = String::from_utf8(session.transport().outbound()[2].clone()).expect("utf8");
        assert!(request.contains(TOOLS_LIST_METHOD));
    }

    #[test]
    fn tools_call_returns_text_content_and_error_flag() {
        let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        transport
            .push_inbound(initialize_ok(MCP_PROTOCOL_VERSION))
            .expect("inbound");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::new(true),
        );
        session.initialize(&CancellationToken::new()).expect("init");
        session
            .transport_mut()
            .push_inbound(
                r#"{"jsonrpc":"2.0","id":2,"result":{"content":[
                    {"type":"text","text":"part one"},
                    {"type":"text","text":"part two"}
                ]}}"#
                    .as_bytes()
                    .to_vec(),
            )
            .expect("inbound");
        let output = session
            .tools_call(
                "echo",
                &serde_json::json!({"message": "hi"}),
                &CancellationToken::new(),
            )
            .expect("call");
        assert_eq!(output.text, "part one\npart two");
        assert!(!output.is_error);
        let request = String::from_utf8(session.transport().outbound()[2].clone()).expect("utf8");
        assert!(request.contains(TOOLS_CALL_METHOD));
        assert!(request.contains("\"echo\""));
    }

    #[test]
    fn tools_call_maps_server_error_to_typed_tool_failed() {
        let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        transport
            .push_inbound(initialize_ok(MCP_PROTOCOL_VERSION))
            .expect("inbound");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::new(true),
        );
        session.initialize(&CancellationToken::new()).expect("init");
        session
            .transport_mut()
            .push_inbound(
                r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"boom"}}"#
                    .as_bytes()
                    .to_vec(),
            )
            .expect("inbound");
        assert_eq!(
            session.tools_call("echo", &serde_json::json!({}), &CancellationToken::new()),
            Err(TransportError::ToolFailed)
        );
    }

    #[test]
    fn tools_list_before_initialize_is_not_initialized() {
        let transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::default(),
        );
        assert_eq!(
            session.tools_list(&CancellationToken::new()),
            Err(TransportError::NotInitialized)
        );
    }

    #[test]
    fn initialize_records_target_version_and_capabilities() {
        let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        transport
            .push_inbound(initialize_ok(MCP_PROTOCOL_VERSION))
            .expect("inbound");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::new(true),
        );
        let handshake = session.initialize(&CancellationToken::new()).expect("init");
        assert_eq!(handshake.protocol_version(), ProtocolVersion::V2026_07_28);
        assert!(handshake.server_capabilities().tools());
        assert!(handshake.server_capabilities().resources());
        assert!(!handshake.server_capabilities().prompts());
        assert_eq!(handshake.server_info().name(), "fixture");
        let outbound = session.transport().outbound();
        assert_eq!(outbound.len(), 2);
        let offer = String::from_utf8(outbound[0].clone()).expect("utf8");
        assert!(offer.contains(MCP_PROTOCOL_VERSION));
        assert!(offer.contains(INITIALIZE_METHOD));
        let ack = String::from_utf8(outbound[1].clone()).expect("utf8");
        assert!(ack.contains(INITIALIZED_METHOD));
    }

    #[test]
    fn initialize_accepts_prior_supported_version() {
        let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        transport
            .push_inbound(initialize_ok(MCP_PRIOR_PROTOCOL_VERSION))
            .expect("inbound");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::default(),
        );
        let handshake = session.initialize(&CancellationToken::new()).expect("init");
        assert_eq!(handshake.protocol_version(), ProtocolVersion::V2025_06_18);
    }

    #[test]
    fn initialize_rejects_unknown_version_without_fallback() {
        let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
        transport
            .push_inbound(initialize_ok("2024-11-05"))
            .expect("inbound");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::default(),
        );
        assert_eq!(
            session.initialize(&CancellationToken::new()),
            Err(TransportError::UnsupportedProtocolVersion)
        );
        assert!(session.handshake().is_none());
    }

    #[test]
    fn stdio_round_trip_is_newline_framed_and_bounded() {
        let incoming = initialize_ok(MCP_PROTOCOL_VERSION);
        let mut framed = incoming.clone();
        framed.push(b'\n');
        let mut transport = StdioTransport::from_pipes(
            Cursor::new(framed),
            Cursor::new(Vec::new()),
            None,
            IoBounds::new(4096, Duration::from_secs(1)).expect("bounds"),
        );
        let got = transport
            .recv_frame(&CancellationToken::new())
            .expect("recv");
        assert_eq!(got, incoming);
        transport
            .send_frame(b"{\"jsonrpc\":\"2.0\"}", &CancellationToken::new())
            .expect("send");
        let written = transport.writer.into_inner();
        assert_eq!(written, b"{\"jsonrpc\":\"2.0\"}\n");
    }

    #[test]
    fn oversized_and_cancelled_stdio_fail_closed() {
        let huge = vec![b'x'; 64];
        let mut framed = huge.clone();
        framed.push(b'\n');
        let mut transport = StdioTransport::from_pipes(
            Cursor::new(framed),
            Cursor::new(Vec::new()),
            None,
            IoBounds::new(16, Duration::from_secs(1)).expect("bounds"),
        );
        assert_eq!(
            transport.recv_frame(&CancellationToken::new()),
            Err(TransportError::FrameTooLarge)
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut transport = StdioTransport::from_pipes(
            Cursor::new(b"{}\n".to_vec()),
            Cursor::new(Vec::new()),
            None,
            IoBounds::standard(),
        );
        assert_eq!(
            transport.send_frame(b"{}", &cancel),
            Err(TransportError::Cancelled)
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_recv_frame_times_out_when_the_peer_never_writes() {
        // A real pipe whose write end stays open but silent — `read()` on
        // the receive end genuinely blocks in the kernel, exactly like a
        // hung MCP stdio server. Without a background reader thread this
        // would hang the test itself; recv_frame must still return promptly.
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().expect("pair");
        let mut transport = StdioTransport::from_pipes(
            reader,
            Cursor::new(Vec::new()),
            None,
            IoBounds::new(4096, Duration::from_millis(200)).expect("bounds"),
        );
        let started = Instant::now();
        assert_eq!(
            transport.recv_frame(&CancellationToken::new()),
            Err(TransportError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A stdio session whose peer is a thread on the other end of a socket:
    /// it answers the handshake, reads the next request line and, when `reply`
    /// is given, answers it after `reply.0` — then reports every line it read
    /// until the session goes away.
    #[cfg(unix)]
    fn socket_session(
        timeout: Duration,
        reply: Option<(Duration, &'static str)>,
    ) -> (
        McpSession<StdioTransport<std::os::unix::net::UnixStream, std::os::unix::net::UnixStream>>,
        thread::JoinHandle<Vec<String>>,
    ) {
        use std::io::{BufRead, BufReader};
        let (client, server) = std::os::unix::net::UnixStream::pair().expect("pair");
        let reader = client.try_clone().expect("clone");
        let mut session = McpSession::new(
            StdioTransport::from_pipes(
                reader,
                client,
                None,
                IoBounds::new(4096, timeout).expect("bounds"),
            ),
            ImplementationInfo::rapidlm(),
            ClientCapabilities::new(true),
        );
        let peer = thread::spawn(move || {
            let mut writer = server.try_clone().expect("clone");
            let mut lines = BufReader::new(server);
            let mut seen = Vec::new();
            // A line, or nothing once the socket has been quiet for the read
            // timeout (the session's own reader thread keeps its end open, so
            // the peer cannot wait for the end of the stream).
            let next = |lines: &mut BufReader<std::os::unix::net::UnixStream>,
                        seen: &mut Vec<String>| {
                let mut line = String::new();
                match BufRead::read_line(lines, &mut line) {
                    Ok(read) if read > 0 => {
                        seen.push(line.trim_end().to_owned());
                        read
                    }
                    _ => 0,
                }
            };
            // initialize, then the initialized notification.
            next(&mut lines, &mut seen);
            let mut frame = initialize_ok(MCP_PROTOCOL_VERSION);
            frame.push(b'\n');
            writer.write_all(&frame).expect("answer initialize");
            next(&mut lines, &mut seen);
            // The request under test.
            next(&mut lines, &mut seen);
            if let Some((after, body)) = reply {
                thread::sleep(after);
                let _ = writer.write_all(format!("{body}\n").as_bytes());
            }
            // Whatever else arrives before the socket goes quiet.
            lines
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(700)))
                .expect("read timeout");
            while next(&mut lines, &mut seen) > 0 {}
            seen
        });
        session.initialize(&CancellationToken::new()).expect("init");
        (session, peer)
    }

    #[cfg(unix)]
    fn cancelled_notices(seen: &[String]) -> Vec<Value> {
        seen.iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["method"] == CANCELLED_METHOD)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_call_that_runs_out_of_time_is_followed_by_a_cancelled_notice_for_its_request() {
        let (mut session, peer) = socket_session(Duration::from_millis(250), None);
        let started = Instant::now();
        let outcome = session.tools_call_with(
            "slow",
            &serde_json::json!({}),
            &CancellationToken::new(),
            &mut |_, _| Err("not expected".to_owned()),
        );
        assert_eq!(outcome, Err(TransportError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(session);
        let seen = peer.join().expect("peer");
        let request: Value = serde_json::from_str(&seen[2]).expect("the call");
        assert_eq!(request["method"], TOOLS_CALL_METHOD);
        let notices = cancelled_notices(&seen);
        assert_eq!(notices.len(), 1, "{seen:?}");
        assert_eq!(notices[0]["params"]["requestId"], request["id"]);
        assert!(
            notices[0].get("id").is_none(),
            "a notice has no id: {:?}",
            notices[0]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_call_the_caller_stops_is_followed_by_a_cancelled_notice_too() {
        let (mut session, peer) = socket_session(Duration::from_secs(5), None);
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            stopper.cancel();
        });
        let outcome =
            session.tools_call_with("slow", &serde_json::json!({}), &cancel, &mut |_, _| {
                Err("not expected".to_owned())
            });
        assert_eq!(outcome, Err(TransportError::Cancelled));
        drop(session);
        let seen = peer.join().expect("peer");
        let notices = cancelled_notices(&seen);
        assert_eq!(notices.len(), 1, "{seen:?}");
        assert!(
            notices[0]["params"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("cancelled")),
            "{:?}",
            notices[0]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_call_that_is_answered_sends_no_notice_and_a_failed_one_does_not_either() {
        let answer =
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"done"}]}}"#;
        let (mut session, peer) = socket_session(
            Duration::from_secs(5),
            Some((Duration::from_millis(50), answer)),
        );
        let output = session
            .tools_call_with(
                "quick",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut |_, _| Err("not expected".to_owned()),
            )
            .expect("answered");
        assert_eq!(output.text, "done");
        drop(session);
        assert!(cancelled_notices(&peer.join().expect("peer")).is_empty());
        // A server that answers with an error was not given up on.
        let failure = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"no"}}"#;
        let (mut session, peer) = socket_session(
            Duration::from_secs(5),
            Some((Duration::from_millis(50), failure)),
        );
        assert_eq!(
            session.tools_call_with(
                "quick",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut |_, _| Err("not expected".to_owned()),
            ),
            Err(TransportError::ToolFailed)
        );
        drop(session);
        assert!(cancelled_notices(&peer.join().expect("peer")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_calls_own_deadline_is_back_after_the_notice_went_out() {
        let (mut session, peer) = socket_session(Duration::from_millis(250), None);
        let _ = session.tools_call_with(
            "slow",
            &serde_json::json!({}),
            &CancellationToken::new(),
            &mut |_, _| Err("not expected".to_owned()),
        );
        assert_eq!(
            session.transport().io_timeout(),
            Some(Duration::from_millis(250)),
            "the one-second deadline of the notice must not stay"
        );
        drop(session);
        peer.join().expect("peer");
    }

    #[cfg(unix)]
    #[test]
    fn a_later_deadline_lets_a_slow_server_answer_and_the_bounds_are_checked() {
        let answer =
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"late"}]}}"#;
        // A server that takes 500 ms against a 150 ms bound fails; after the
        // deadline is raised for the call, the same kind of server answers.
        let (mut session, peer) = socket_session(
            Duration::from_millis(150),
            Some((Duration::from_millis(500), answer)),
        );
        assert_eq!(
            session.tools_call_with(
                "slow",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut |_, _| Err("not expected".to_owned()),
            ),
            Err(TransportError::Timeout)
        );
        drop(session);
        peer.join().expect("peer");
        let (mut session, peer) = socket_session(
            Duration::from_millis(150),
            Some((Duration::from_millis(500), answer)),
        );
        session
            .set_io_timeout(Duration::from_secs(5))
            .expect("raised");
        let output = session
            .tools_call_with(
                "slow",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut |_, _| Err("not expected".to_owned()),
            )
            .expect("answers within the later deadline");
        assert_eq!(output.text, "late");
        assert_eq!(
            session.set_io_timeout(Duration::ZERO),
            Err(TransportError::InvalidBounds)
        );
        assert_eq!(
            session.set_io_timeout(MAX_IO_TIMEOUT + Duration::from_secs(1)),
            Err(TransportError::InvalidBounds)
        );
        assert_eq!(session.set_io_timeout(MAX_IO_TIMEOUT), Ok(()));
        drop(session);
        peer.join().expect("peer");
    }

    #[cfg(unix)]
    #[test]
    fn stdio_recv_frame_is_interrupted_by_cancellation_when_the_peer_never_writes() {
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().expect("pair");
        let mut transport =
            StdioTransport::from_pipes(reader, Cursor::new(Vec::new()), None, IoBounds::standard());
        let cancel = CancellationToken::new();
        let watchdog = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            watchdog.cancel();
        });
        let started = Instant::now();
        assert_eq!(
            transport.recv_frame(&cancel),
            Err(TransportError::Cancelled)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn http_connect_requires_tool_egress_and_net_connect_scope() {
        let err = StreamableHttpTransport::connect(
            RecordedHttp::default(),
            MapResolver::fixture(),
            EgressProxy::new(EgressPolicy::none(), NetworkClient::Tool),
            HttpConnectRequest::new(
                "https://mcp.example.com/mcp",
                Capability::NetConnect,
                net_resource("mcp.example.com", 443),
                None,
                IoBounds::standard(),
            ),
            &CancellationToken::new(),
        )
        .expect_err("denied");
        assert_eq!(err, TransportError::NetworkDenied);

        let err = connect_http(
            RecordedHttp::default(),
            "https://mcp.example.com/mcp",
            "mcp.example.com",
            None,
        );
        // connect_http allowlists mcp.example.com, so this should succeed.
        err.expect("allowlisted connect");

        let err = StreamableHttpTransport::connect(
            RecordedHttp::default(),
            MapResolver::fixture(),
            tool_proxy(&["mcp.example.com"]),
            HttpConnectRequest::new(
                "https://mcp.example.com/mcp",
                Capability::McpInvoke,
                net_resource("mcp.example.com", 443),
                None,
                IoBounds::standard(),
            ),
            &CancellationToken::new(),
        )
        .expect_err("wrong cap");
        assert_eq!(err, TransportError::CapabilityDenied);

        let err = StreamableHttpTransport::connect(
            RecordedHttp::default(),
            MapResolver::fixture(),
            tool_proxy(&["mcp.example.com", "evil.example.com"]),
            HttpConnectRequest::new(
                "https://evil.example.com/mcp",
                Capability::NetConnect,
                net_resource("mcp.example.com", 443),
                None,
                IoBounds::standard(),
            ),
            &CancellationToken::new(),
        )
        .expect_err("scope mismatch");
        assert_eq!(err, TransportError::NetworkScopeDenied);
    }

    #[test]
    fn http_initialize_records_version_and_reauthorizes_each_post() {
        let io = RecordedHttp::default();
        io.push(
            HttpResponse::new(
                200,
                Some("sess-1".to_owned()),
                None,
                initialize_ok(MCP_PROTOCOL_VERSION),
            )
            .expect("resp"),
        );
        io.push(HttpResponse::new(200, None, None, Vec::new()).expect("ack"));
        let transport = connect_http(
            io.clone(),
            "https://mcp.example.com/mcp",
            "mcp.example.com",
            None,
        )
        .expect("connect");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::default(),
        );
        let handshake = session.initialize(&CancellationToken::new()).expect("init");
        assert_eq!(handshake.protocol_version(), ProtocolVersion::V2026_07_28);
        assert_eq!(
            handshake.session_id().map(McpSessionId::as_str),
            Some("sess-1")
        );
        assert_eq!(io.posts().len(), 2);
    }

    #[test]
    fn http_credential_scope_must_match_authorized_origin() {
        let handle = SecretRef::from_alias("env:MCP_TOKEN").expect("ref");
        let other = canonical_https("evil.example.com");
        let credential = HttpAuthScope::new(handle.clone(), &other).expect("scope");
        let err = connect_http(
            RecordedHttp::default(),
            "https://mcp.example.com/mcp",
            "mcp.example.com",
            Some(credential),
        )
        .expect_err("mismatch");
        assert_eq!(err, TransportError::CredentialScopeDenied);

        let target = canonical_https("mcp.example.com");
        let credential = HttpAuthScope::new(handle, &target).expect("scope");
        let debug = format!("{credential:?}");
        assert!(debug.contains("redacted"));
        assert!(!debug.contains("env:MCP_TOKEN"));
        connect_http(
            RecordedHttp::default(),
            "https://mcp.example.com/mcp",
            "mcp.example.com",
            Some(credential),
        )
        .expect("matching scope");
    }

    #[test]
    fn http_rejects_userinfo_loopback_and_metadata_without_explicit_rule() {
        let err = StreamableHttpTransport::connect(
            RecordedHttp::default(),
            MapResolver::fixture(),
            tool_proxy(&["mcp.example.com"]),
            HttpConnectRequest::new(
                "https://user:token@mcp.example.com/mcp",
                Capability::NetConnect,
                net_resource("mcp.example.com", 443),
                None,
                IoBounds::standard(),
            ),
            &CancellationToken::new(),
        )
        .expect_err("userinfo");
        assert_eq!(err, TransportError::NetworkDenied);

        let err = StreamableHttpTransport::connect(
            RecordedHttp::default(),
            MapResolver::fixture(),
            tool_proxy(&["mcp.example.com"]),
            HttpConnectRequest::new(
                "https://localhost/mcp",
                Capability::NetConnect,
                NetworkScope::new(NetworkScheme::Https, "localhost", 443)
                    .map(ResourceDescriptor::Network)
                    .expect("scope"),
                None,
                IoBounds::standard(),
            ),
            &CancellationToken::new(),
        )
        .expect_err("loopback");
        assert_eq!(err, TransportError::NetworkDenied);

        let err = StreamableHttpTransport::connect(
            RecordedHttp::default(),
            MapResolver::fixture(),
            tool_proxy(&["mcp.example.com"]),
            HttpConnectRequest::new(
                "https://metadata.google.internal/latest/meta-data",
                Capability::NetConnect,
                NetworkScope::new(NetworkScheme::Https, "metadata.google.internal", 443)
                    .map(ResourceDescriptor::Network)
                    .expect("scope"),
                None,
                IoBounds::standard(),
            ),
            &CancellationToken::new(),
        )
        .expect_err("metadata");
        assert_eq!(err, TransportError::NetworkDenied);
    }

    #[test]
    fn http_redirect_to_unscoped_host_is_denied() {
        let io = RecordedHttp::default();
        io.push(
            HttpResponse::new(
                302,
                None,
                Some("https://evil.example.com/mcp".to_owned()),
                Vec::new(),
            )
            .expect("redirect"),
        );
        let transport = connect_http(io, "https://mcp.example.com/mcp", "mcp.example.com", None)
            .expect("connect");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::default(),
        );
        assert_eq!(
            session.initialize(&CancellationToken::new()),
            Err(TransportError::NetworkScopeDenied)
        );
    }

    #[test]
    fn cancelled_http_initialize_and_closed_session_fail() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = StreamableHttpTransport::connect(
            RecordedHttp::default(),
            MapResolver::fixture(),
            tool_proxy(&["mcp.example.com"]),
            HttpConnectRequest::new(
                "https://mcp.example.com/mcp",
                Capability::NetConnect,
                net_resource("mcp.example.com", 443),
                None,
                IoBounds::standard(),
            ),
            &cancel,
        )
        .expect_err("cancel");
        assert_eq!(err, TransportError::Cancelled);
    }

    #[test]
    fn errors_and_debug_do_not_echo_url_or_secret() {
        let err = TransportError::NetworkDenied;
        let text = err.to_string();
        assert!(!text.contains("example.com"));
        assert!(!text.contains("token"));
        let api = err.into_api_error(TraceId::new()).expect("api");
        assert_eq!(api.code(), ErrorCode::PolicyDenied);
        let json = format!("{api:?}");
        assert!(!json.contains("password"));
    }

    fn json_reply(body: &str) -> HttpResponse {
        HttpResponse::new(200, None, None, body.as_bytes().to_vec()).expect("reply")
    }

    fn stream_reply(body: &str) -> HttpResponse {
        json_reply(body).with_content_type(Some("text/event-stream; charset=utf-8".to_owned()))
    }

    /// An initialized session over a scripted HTTP io; the initialize reply
    /// and the `202` that acknowledges `notifications/initialized` are queued.
    fn initialized_http_session(
        io: &RecordedHttp,
        bounds: IoBounds,
    ) -> McpSession<StreamableHttpTransport<RecordedHttp, MapResolver>> {
        io.push(
            HttpResponse::new(
                200,
                Some("sess-1".to_owned()),
                None,
                initialize_ok(MCP_PROTOCOL_VERSION),
            )
            .expect("init"),
        );
        io.push(HttpResponse::new(202, None, None, Vec::new()).expect("ack"));
        let transport = StreamableHttpTransport::connect(
            io.clone(),
            MapResolver::fixture(),
            tool_proxy(&["mcp.example.com"]),
            HttpConnectRequest::new(
                "https://mcp.example.com/mcp",
                Capability::NetConnect,
                net_resource("mcp.example.com", 443),
                None,
                bounds,
            ),
            &CancellationToken::new(),
        )
        .expect("connect");
        let mut session = McpSession::new(
            transport,
            ImplementationInfo::rapidlm(),
            ClientCapabilities::default(),
        );
        session
            .initialize(&CancellationToken::new())
            .expect("a 202 acknowledging the initialized notification is a success");
        session
    }

    const CALL_RESULT: &str =
        r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hello"}]}}"#;

    fn no_server_requests() -> impl FnMut(&str, &Value) -> Result<Value, String> {
        |_method, _params| Err("not expected".to_owned())
    }

    #[test]
    fn http_tools_call_with_returns_a_json_reply() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        io.push(json_reply(CALL_RESULT));
        let out = session
            .tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut no_server_requests(),
            )
            .expect("call over http");
        assert_eq!(out.text, "hello");
        assert!(!out.is_error);
    }

    #[test]
    fn http_tools_call_with_reads_an_event_stream_reply() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        let progress =
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1}}"#;
        let stale = r#"{"jsonrpc":"2.0","id":99,"result":{}}"#;
        io.push(stream_reply(&format!(
            ": keep-alive\r\nevent: message\r\ndata: {progress}\r\n\r\nid: 7\r\ndata: {stale}\r\n\r\ndata: {CALL_RESULT}\r\n\r\n"
        )));
        let out = session
            .tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut no_server_requests(),
            )
            .expect("event-stream reply");
        assert_eq!(
            out.text, "hello",
            "the id-matched event wins over the others"
        );
    }

    #[test]
    fn http_tools_call_over_an_event_stream_skips_other_events() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        let request = r#"{"jsonrpc":"2.0","id":7,"method":"roots/list"}"#;
        io.push(stream_reply(&format!(
            "data: {request}\n\ndata: {CALL_RESULT}\n\n"
        )));
        let out = session
            .tools_call("echo", &serde_json::json!({}), &CancellationToken::new())
            .expect("stream");
        assert_eq!(out.text, "hello");
    }

    #[test]
    fn http_server_request_inside_a_stream_is_answered_with_a_post() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        let ask = r#"{"jsonrpc":"2.0","id":"s1","method":"elicitation/create","params":{}}"#;
        io.push(stream_reply(&format!(
            "data: {ask}\n\ndata: {CALL_RESULT}\n\n"
        )));
        io.push(HttpResponse::new(202, None, None, Vec::new()).expect("ack"));
        let mut answered = Vec::new();
        let out = session
            .tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut |method: &str, _params: &Value| {
                    answered.push(method.to_owned());
                    Ok(serde_json::json!({"action":"cancel"}))
                },
            )
            .expect("call");
        assert_eq!(out.text, "hello");
        assert_eq!(answered, vec!["elicitation/create".to_owned()]);
        assert_eq!(
            io.posts().len(),
            4,
            "initialize, initialized, tools/call, and the answer to the server request"
        );
    }

    #[test]
    fn http_reply_without_a_matching_response_is_an_invalid_frame() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        io.push(stream_reply(
            "data: {\"jsonrpc\":\"2.0\",\"id\":41,\"result\":{}}\n\n",
        ));
        assert_eq!(
            session.tools_call("echo", &serde_json::json!({}), &CancellationToken::new()),
            Err(TransportError::InvalidFrame)
        );
        io.push(HttpResponse::new(202, None, None, Vec::new()).expect("empty"));
        assert_eq!(
            session.tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut no_server_requests(),
            ),
            Err(TransportError::InvalidFrame),
            "a request answered with an empty 202 has no reply to read"
        );
    }

    #[test]
    fn http_unread_frames_of_an_earlier_exchange_do_not_leak_into_the_next_call() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        // The result arrives first and the call returns; a server request that
        // follows it in the same stream is left unread.
        let leftover = r#"{"jsonrpc":"2.0","id":"late","method":"roots/list"}"#;
        io.push(stream_reply(&format!(
            "data: {CALL_RESULT}\n\ndata: {leftover}\n\n"
        )));
        session
            .tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut no_server_requests(),
            )
            .expect("first call");
        let next = CALL_RESULT.replace("\"id\":2", "\"id\":3");
        io.push(json_reply(&next));
        let mut asked = Vec::new();
        session
            .tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut |method: &str, _params: &Value| {
                    asked.push(method.to_owned());
                    Err("not expected".to_owned())
                },
            )
            .expect("second call");
        assert!(
            asked.is_empty(),
            "a new request starts a fresh exchange; the earlier stream's leftovers are dropped: {asked:?}"
        );
    }

    #[test]
    fn http_frame_bounds_apply_to_json_bodies_and_to_each_stream_event() {
        let bounds = IoBounds::new(256, Duration::from_secs(5)).expect("bounds");
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, bounds);
        let big = format!(
            r#"{{"jsonrpc":"2.0","id":2,"result":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(400)
        );
        io.push(json_reply(&big));
        assert_eq!(
            session.tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut no_server_requests(),
            ),
            Err(TransportError::FrameTooLarge)
        );
        io.push(stream_reply(&format!("data: {big}\n\n")));
        assert_eq!(
            session.tools_call_with(
                "echo",
                &serde_json::json!({}),
                &CancellationToken::new(),
                &mut no_server_requests(),
            ),
            Err(TransportError::FrameTooLarge),
            "one event over the frame bound is refused even inside a larger stream bound"
        );
        assert!(
            HttpResponse::new(200, None, None, vec![b'x'; MAX_EVENT_STREAM_BYTES + 1]).is_err(),
            "the whole-stream bound is enforced when the response is built"
        );
    }

    #[test]
    fn http_401_is_unauthorized_and_keeps_a_parsed_challenge() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        io.push(
            HttpResponse::new(401, None, None, Vec::new())
                .expect("401")
                .with_www_authenticate(Some(
                    r#"Bearer realm="mcp", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="read write", error="invalid_token""#
                        .to_owned(),
                )),
        );
        assert_eq!(
            session.tools_list(&CancellationToken::new()),
            Err(TransportError::Unauthorized)
        );
        let challenge = session
            .transport()
            .auth_challenge()
            .expect("challenge kept")
            .clone();
        assert_eq!(
            challenge.resource_metadata.as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(challenge.scope.as_deref(), Some("read write"));
        assert_eq!(challenge.error.as_deref(), Some("invalid_token"));

        io.push(json_reply(
            r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#,
        ));
        session
            .tools_list(&CancellationToken::new())
            .expect("a later success");
        assert!(
            session.transport().auth_challenge().is_none(),
            "a success clears the challenge"
        );
    }

    #[test]
    fn http_403_is_insufficient_scope_only_when_the_challenge_says_so() {
        let io = RecordedHttp::default();
        let mut session = initialized_http_session(&io, IoBounds::standard());
        io.push(
            HttpResponse::new(403, None, None, Vec::new())
                .expect("403")
                .with_www_authenticate(Some(
                    r#"Bearer error="insufficient_scope", scope="admin""#.to_owned(),
                )),
        );
        assert_eq!(
            session.tools_list(&CancellationToken::new()),
            Err(TransportError::InsufficientScope)
        );
        assert_eq!(
            session
                .transport()
                .auth_challenge()
                .and_then(|c| c.scope.as_deref()),
            Some("admin")
        );
        io.push(HttpResponse::new(403, None, None, Vec::new()).expect("plain 403"));
        assert_eq!(
            session.tools_list(&CancellationToken::new()),
            Err(TransportError::HandshakeFailed),
            "a 403 that is not about scope stays a plain failure"
        );
        io.push(HttpResponse::new(500, None, None, Vec::new()).expect("500"));
        assert_eq!(
            session.tools_list(&CancellationToken::new()),
            Err(TransportError::HandshakeFailed)
        );
        assert_eq!(
            TransportError::Unauthorized.code(),
            Some(ErrorCode::AuthRequired)
        );
    }

    #[test]
    fn www_authenticate_parsing_is_bounded_and_tolerant() {
        let parsed = parse_www_authenticate(
            r#"Newauth realm="x", Bearer ERROR=invalid_token, Scope="a b", resource_metadata=https://rs.example/meta"#,
        );
        assert_eq!(parsed.error.as_deref(), Some("invalid_token"));
        assert_eq!(parsed.scope.as_deref(), Some("a b"));
        assert_eq!(
            parsed.resource_metadata.as_deref(),
            Some("https://rs.example/meta"),
            "parameter names are case-insensitive and unquoted tokens are accepted"
        );

        let escaped = parse_www_authenticate(r#"Bearer scope="a\"b""#);
        assert_eq!(escaped.scope.as_deref(), Some("a\"b"));

        let first_wins = parse_www_authenticate(r#"Bearer error="one", error="two""#);
        assert_eq!(first_wins.error.as_deref(), Some("one"));

        assert_eq!(
            parse_www_authenticate(r#"Bearer scope="unterminated"#),
            AuthChallenge::default(),
            "an unterminated quote yields nothing rather than a guess"
        );
        assert_eq!(
            parse_www_authenticate("Bearer scope=\"a\u{7}b\""),
            AuthChallenge::default(),
            "control characters void the parameter"
        );
        assert_eq!(parse_www_authenticate(""), AuthChallenge::default());
        assert_eq!(
            parse_www_authenticate("Bearer"),
            AuthChallenge::default(),
            "a bare scheme carries no parameters"
        );

        let oversized = format!(r#"Bearer scope="{}""#, "s".repeat(MAX_CHALLENGE_BYTES));
        assert_eq!(parse_www_authenticate(&oversized), AuthChallenge::default());
        let long_param = format!(r#"Bearer scope="{}", error=ok"#, "s".repeat(1500));
        let parsed = parse_www_authenticate(&long_param);
        assert_eq!(parsed.scope, None, "a parameter over 1 KiB is dropped");
        assert_eq!(parsed.error.as_deref(), Some("ok"));
    }

    #[test]
    fn event_stream_parsing_follows_the_line_and_event_rules() {
        let frames = |text: &str| -> Vec<String> {
            parse_event_stream(text.as_bytes())
                .into_iter()
                .map(|f| String::from_utf8(f).expect("utf8"))
                .collect()
        };
        assert_eq!(
            frames("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"),
            vec!["{\"a\":1}", "{\"b\":2}"]
        );
        assert_eq!(
            frames("data: {\"a\":\r\ndata: 1}\r\n\r\n"),
            vec!["{\"a\":\n1}"],
            "multi-line data joins with a newline; CRLF is a line end"
        );
        assert_eq!(frames("data: 1\r\r"), vec!["1"], "a lone CR ends a line");
        assert_eq!(
            frames(
                ": c\nid: 5\nretry: 10\nevent: ping\ndata: 1\n\nevent: message\ndata: 2\n\ndata: 3\n\n"
            ),
            vec!["2", "3"],
            "comments, id and retry are ignored; a non-message event is dropped"
        );
        assert_eq!(
            frames("\u{feff}data: 1\n\n"),
            vec!["1"],
            "a leading BOM is skipped"
        );
        assert_eq!(
            frames("data:1\n\n"),
            vec!["1"],
            "the space after the colon is optional"
        );
        assert_eq!(
            frames("data: {\"a\":1}\n\ndata: {\"b\":"),
            vec!["{\"a\":1}"],
            "an unterminated final event that is not complete JSON is not dispatched"
        );
        assert_eq!(
            frames("data: {\"a\":1}"),
            vec!["{\"a\":1}"],
            "an unterminated final event that is complete JSON is kept"
        );
        assert!(
            frames("event: message\n\n").is_empty(),
            "an event without data is empty"
        );
        assert!(frames("").is_empty());
    }

    #[test]
    fn an_event_with_an_empty_data_field_is_not_dispatched() {
        // A priming event (`data:` with nothing after it) carries no message.
        let frames = parse_event_stream(b"data:\n\ndata: {\"a\":1}\n\ndata: \n\n");
        assert_eq!(frames, vec![b"{\"a\":1}".to_vec()]);
        assert!(parse_event_stream(b"data\n\n").is_empty());
    }

    #[test]
    fn the_challenge_bounds_are_inclusive() {
        let at_header_limit = format!("Bearer error=\"{}\"", "e".repeat(1024));
        assert!(at_header_limit.len() < MAX_CHALLENGE_BYTES);
        assert_eq!(
            parse_www_authenticate(&at_header_limit)
                .error
                .map(|e| e.len()),
            Some(1024),
            "a parameter of exactly 1 KiB is kept"
        );
        let over = format!("Bearer error=\"{}\"", "e".repeat(1025));
        assert_eq!(
            parse_www_authenticate(&over).error,
            None,
            "one byte more is dropped"
        );
        // The whole header: exactly 2048 bytes is parsed, 2049 is dropped whole.
        let pad = |total: usize| {
            let prefix = "Bearer scope=\"s\", x=\"";
            let suffix = "\"";
            format!(
                "{prefix}{}{suffix}",
                "p".repeat(total - prefix.len() - suffix.len())
            )
        };
        assert_eq!(
            parse_www_authenticate(&pad(MAX_CHALLENGE_BYTES))
                .scope
                .as_deref(),
            Some("s")
        );
        assert_eq!(
            parse_www_authenticate(&pad(MAX_CHALLENGE_BYTES + 1)),
            AuthChallenge::default()
        );
    }

    #[test]
    fn content_type_decides_framing_not_the_body_shape() {
        // A JSON body whose text happens to look like an event stream is one frame.
        let body = b"data: {\"a\":1}\n\n";
        let frames = split_reply_frames(Some("application/json"), body, 1024).expect("frames");
        assert_eq!(frames, vec![body.to_vec()]);
        // And an event stream is only recognised by its media type.
        let frames = split_reply_frames(Some("Text/Event-Stream;charset=utf-8"), body, 1024)
            .expect("frames");
        assert_eq!(frames, vec![b"{\"a\":1}".to_vec()]);
        assert!(
            split_reply_frames(None, b"", 1024)
                .expect("empty")
                .is_empty()
        );
    }

    fn canonical_https(host: &str) -> CanonicalNetworkTarget {
        let resolver = MapResolver::fixture();
        normalize_network(
            &NetworkIntent::connect(format!("https://{host}:443")),
            &resolver,
            &CancellationToken::new(),
        )
        .expect("normalize")
    }
}
