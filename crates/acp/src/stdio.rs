//! JSON-RPC framing/session transport over stdio with exclusive stdout ownership.
//!
//! ACP messages are newline-delimited UTF-8 JSON-RPC 2.0 objects. Stdout may
//! contain only those frames. Diagnostics belong on stderr. The kernel is not
//! invoked for framing faults.

use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 version written on every constructed frame.
pub const JSONRPC_VERSION: &str = "2.0";

/// Maximum UTF-8 bytes of one compact JSON object (excluding the trailing newline).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Maximum UTF-8 bytes accepted in one diagnostic line.
pub const MAX_DIAGNOSTIC_LINE_BYTES: usize = 8 * 1024;

/// JSON-RPC parse error.
pub const PARSE_ERROR: i64 = -32700;

/// JSON-RPC invalid request.
pub const INVALID_REQUEST: i64 = -32600;

/// JSON-RPC method not found.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC invalid params.
pub const INVALID_PARAMS: i64 = -32602;

/// JSON-RPC internal error.
pub const INTERNAL_ERROR: i64 = -32603;

const READ_WINDOW: usize = 8 * 1024;

const PARSE_ERROR_MESSAGE: &str = "Parse error";
const INVALID_REQUEST_MESSAGE: &str = "Invalid Request";
const FRAME_TOO_LARGE_MESSAGE: &str = "Frame too large";

/// Cooperative cancellation for read/write loops. Callers must not detach I/O.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Hard ceilings for one transport instance. Larger values fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportLimits {
    max_frame_bytes: usize,
    max_diagnostic_bytes: usize,
}

/// Why a connected transport became disconnected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DisconnectReason {
    Explicit,
    PeerClosed,
    Cancelled,
    UnrecoverableFrame,
    Io,
}

/// Session lifecycle of one stdio transport. Writes are refused after disconnect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TransportState {
    Connected,
    Disconnected(DisconnectReason),
}

/// Recoverable inbound framing fault. A JSON-RPC error frame is written.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProtocolFault {
    ParseError,
    InvalidRequest,
    FrameTooLarge,
}

/// Unrecoverable inbound framing. The transport disconnects; the kernel is not crashed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FrameFault {
    TooLarge,
    Incomplete,
}

/// One successfully framed inbound item, or a handled protocol fault.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Message(JsonRpcMessage),
    ProtocolFault(ProtocolFault),
}

/// JSON-RPC 2.0 request id. Notifications omit this field entirely.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
    Null,
}

/// JSON-RPC 2.0 error object. Display never echoes `data`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonRpcErrorObject {
    code: i64,
    message: String,
    data: Option<Value>,
}

/// One ACP JSON-RPC frame. Compact encoding contains no embedded newlines.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonRpcMessage {
    Request {
        id: JsonRpcId,
        method: String,
        params: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
    Result {
        id: JsonRpcId,
        result: Value,
    },
    Error {
        id: JsonRpcId,
        error: JsonRpcErrorObject,
    },
}

/// Typed stdio transport failure. Inbound payload bytes are never stored.
#[derive(Debug)]
pub enum StdioError {
    Cancelled,
    Disconnected { reason: DisconnectReason },
    UnrecoverableFrame { kind: FrameFault },
    LimitInvalid,
    InvalidOutbound,
    EmbeddedNewline,
    FrameTooLarge { limit: usize, observed: usize },
    DiagnosticNotSingleLine,
    EmptyDiagnostic,
    Encode(serde_json::Error),
    Io(io::Error),
}

/// Newline-delimited frame reader. Leftover bytes stay in an internal stash.
pub struct FrameReader<R> {
    inner: R,
    stash: Vec<u8>,
    max_frame_bytes: usize,
}

/// Exclusive protocol writer. Callers must not write to the same stdout handle.
pub struct FrameWriter<W> {
    inner: W,
    max_frame_bytes: usize,
}

/// Human diagnostics. Never writes to the protocol writer.
pub struct StderrLog<W> {
    err: W,
    cancel: CancellationToken,
    max_diagnostic_bytes: usize,
}

/// JSON-RPC session transport over a pair of byte streams.
pub struct StdioTransport<R, W> {
    reader: FrameReader<R>,
    writer: FrameWriter<W>,
    state: TransportState,
    cancel: CancellationToken,
    limits: TransportLimits,
}

/// Protocol transport plus a stderr-only diagnostic sink.
pub struct StdioSession<R, W, E> {
    transport: StdioTransport<R, W>,
    log: StderrLog<E>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), StdioError> {
        if self.is_cancelled() {
            Err(StdioError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl TransportLimits {
    pub const fn default_limits() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_diagnostic_bytes: MAX_DIAGNOSTIC_LINE_BYTES,
        }
    }

    pub fn new(max_frame_bytes: usize, max_diagnostic_bytes: usize) -> Result<Self, StdioError> {
        if max_frame_bytes == 0 || max_frame_bytes > MAX_FRAME_BYTES {
            return Err(StdioError::LimitInvalid);
        }
        if max_diagnostic_bytes == 0 || max_diagnostic_bytes > MAX_DIAGNOSTIC_LINE_BYTES {
            return Err(StdioError::LimitInvalid);
        }
        Ok(Self {
            max_frame_bytes,
            max_diagnostic_bytes,
        })
    }

    pub const fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    pub const fn max_diagnostic_bytes(self) -> usize {
        self.max_diagnostic_bytes
    }
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self::default_limits()
    }
}

impl DisconnectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::PeerClosed => "peer_closed",
            Self::Cancelled => "cancelled",
            Self::UnrecoverableFrame => "unrecoverable_frame",
            Self::Io => "io",
        }
    }
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ProtocolFault {
    pub const fn code(self) -> i64 {
        match self {
            Self::ParseError => PARSE_ERROR,
            Self::InvalidRequest | Self::FrameTooLarge => INVALID_REQUEST,
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::ParseError => PARSE_ERROR_MESSAGE,
            Self::InvalidRequest => INVALID_REQUEST_MESSAGE,
            Self::FrameTooLarge => FRAME_TOO_LARGE_MESSAGE,
        }
    }
}

impl JsonRpcId {
    fn to_value(&self) -> Value {
        match self {
            Self::Number(n) => Value::Number((*n).into()),
            Self::String(s) => Value::String(s.clone()),
            Self::Null => Value::Null,
        }
    }

    fn from_value(value: &Value) -> Result<Self, ProtocolFault> {
        match value {
            Value::Null => Ok(Self::Null),
            Value::Number(n) => n
                .as_i64()
                .map(Self::Number)
                .ok_or(ProtocolFault::InvalidRequest),
            Value::String(s) => Ok(Self::String(s.clone())),
            _ => Err(ProtocolFault::InvalidRequest),
        }
    }
}

impl Serialize for JsonRpcId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonRpcId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(&value).map_err(de::Error::custom)
    }
}

impl JsonRpcErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn code(&self) -> i64 {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }

    fn from_value(value: &Value) -> Result<Self, ProtocolFault> {
        let obj = value.as_object().ok_or(ProtocolFault::InvalidRequest)?;
        let code = match obj.get("code") {
            Some(Value::Number(n)) => n.as_i64().ok_or(ProtocolFault::InvalidRequest)?,
            _ => return Err(ProtocolFault::InvalidRequest),
        };
        let message = match obj.get("message") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err(ProtocolFault::InvalidRequest),
        };
        Ok(Self {
            code,
            message,
            data: obj.get("data").cloned(),
        })
    }
}

impl JsonRpcMessage {
    pub fn protocol_error(fault: ProtocolFault) -> Self {
        Self::Error {
            id: JsonRpcId::Null,
            error: JsonRpcErrorObject::new(fault.code(), fault.message()),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, StdioError> {
        encode_message(self, MAX_FRAME_BYTES)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolFault> {
        decode_message(bytes)
    }
}

impl Serialize for JsonRpcErrorObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let n = 2 + usize::from(self.data.is_some());
        let mut state = serializer.serialize_struct("JsonRpcErrorObject", n)?;
        state.serialize_field("code", &self.code)?;
        state.serialize_field("message", &self.message)?;
        if let Some(data) = &self.data {
            state.serialize_field("data", data)?;
        }
        state.end()
    }
}

impl Serialize for JsonRpcMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        validate_outbound(self).map_err(serde::ser::Error::custom)?;
        match self {
            Self::Request { id, method, params } => {
                let n = 3 + usize::from(params.is_some());
                let mut state = serializer.serialize_struct("JsonRpcMessage", n)?;
                state.serialize_field("jsonrpc", JSONRPC_VERSION)?;
                state.serialize_field("id", id)?;
                state.serialize_field("method", method)?;
                if let Some(params) = params {
                    state.serialize_field("params", params)?;
                }
                state.end()
            }
            Self::Notification { method, params } => {
                let n = 2 + usize::from(params.is_some());
                let mut state = serializer.serialize_struct("JsonRpcMessage", n)?;
                state.serialize_field("jsonrpc", JSONRPC_VERSION)?;
                state.serialize_field("method", method)?;
                if let Some(params) = params {
                    state.serialize_field("params", params)?;
                }
                state.end()
            }
            Self::Result { id, result } => {
                let mut state = serializer.serialize_struct("JsonRpcMessage", 3)?;
                state.serialize_field("jsonrpc", JSONRPC_VERSION)?;
                state.serialize_field("id", id)?;
                state.serialize_field("result", result)?;
                state.end()
            }
            Self::Error { id, error } => {
                let mut state = serializer.serialize_struct("JsonRpcMessage", 3)?;
                state.serialize_field("jsonrpc", JSONRPC_VERSION)?;
                state.serialize_field("id", id)?;
                state.serialize_field("error", error)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for JsonRpcMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        decode_value(value).map_err(de::Error::custom)
    }
}

impl fmt::Display for ProtocolFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl fmt::Display for FrameFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "frame exceeds the byte bound without a delimiter",
            Self::Incomplete => "peer closed before the frame delimiter",
        })
    }
}

impl fmt::Display for StdioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("acp stdio transport cancelled"),
            Self::Disconnected { reason } => {
                write!(f, "acp stdio transport disconnected ({reason})")
            }
            Self::UnrecoverableFrame { kind } => {
                write!(f, "acp stdio framing cannot recover ({kind})")
            }
            Self::LimitInvalid => f.write_str("acp stdio transport limit is invalid"),
            Self::InvalidOutbound => f.write_str("outbound acp message is not valid json-rpc"),
            Self::EmbeddedNewline => f.write_str("encoded acp frame contains an embedded newline"),
            Self::FrameTooLarge { limit, observed } => {
                write!(
                    f,
                    "acp stdio frame exceeds {limit} bytes (observed {observed})"
                )
            }
            Self::DiagnosticNotSingleLine => f.write_str("acp diagnostic must be a single line"),
            Self::EmptyDiagnostic => f.write_str("acp diagnostic must be non-empty"),
            Self::Encode(err) => write!(f, "acp stdio encode failed: {err}"),
            Self::Io(err) => write!(f, "acp stdio i/o failed: {err}"),
        }
    }
}

impl Error for StdioError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R, max_frame_bytes: usize) -> Result<Self, StdioError> {
        if max_frame_bytes == 0 || max_frame_bytes > MAX_FRAME_BYTES {
            return Err(StdioError::LimitInvalid);
        }
        Ok(Self {
            inner,
            stash: Vec::new(),
            max_frame_bytes,
        })
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read one newline-delimited payload. `Ok(None)` is a clean EOF with no leftover.
    pub fn read_frame(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, StdioError> {
        loop {
            cancel.check()?;
            if let Some(line) = take_complete_line(&mut self.stash) {
                if line.is_empty() {
                    continue;
                }
                return Ok(Some(line));
            }
            let max_stash = self.max_frame_bytes.saturating_add(1);
            if self.stash.len() >= max_stash {
                if self.discard_through_delimiter(cancel)? {
                    return Err(StdioError::FrameTooLarge {
                        limit: self.max_frame_bytes,
                        observed: max_stash,
                    });
                }
                return Err(StdioError::UnrecoverableFrame {
                    kind: FrameFault::TooLarge,
                });
            }
            let remaining = max_stash - self.stash.len();
            let mut buf = vec![0u8; remaining.min(READ_WINDOW)];
            match self.inner.read(&mut buf) {
                Ok(0) => {
                    if self.stash.is_empty() {
                        return Ok(None);
                    }
                    return Err(StdioError::UnrecoverableFrame {
                        kind: FrameFault::Incomplete,
                    });
                }
                Ok(n) => self.stash.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(StdioError::Io(err)),
            }
        }
    }

    fn discard_through_delimiter(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<bool, StdioError> {
        if let Some(idx) = self.stash.iter().position(|&b| b == b'\n') {
            let _ = self.stash.drain(..=idx);
            return Ok(true);
        }
        self.stash.clear();
        loop {
            cancel.check()?;
            let mut buf = [0u8; READ_WINDOW];
            match self.inner.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    if let Some(idx) = buf[..n].iter().position(|&b| b == b'\n') {
                        self.stash.extend_from_slice(&buf[idx + 1..n]);
                        return Ok(true);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(StdioError::Io(err)),
            }
        }
    }
}

impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W, max_frame_bytes: usize) -> Result<Self, StdioError> {
        if max_frame_bytes == 0 || max_frame_bytes > MAX_FRAME_BYTES {
            return Err(StdioError::LimitInvalid);
        }
        Ok(Self {
            inner,
            max_frame_bytes,
        })
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Write one compact JSON-RPC object and a trailing newline, then flush.
    pub fn write_message(
        &mut self,
        message: &JsonRpcMessage,
        cancel: &CancellationToken,
    ) -> Result<(), StdioError> {
        cancel.check()?;
        let buf = encode_message(message, self.max_frame_bytes)?;
        self.inner.write_all(&buf).map_err(StdioError::Io)?;
        self.inner.flush().map_err(StdioError::Io)?;
        Ok(())
    }
}

impl<W: Write> StderrLog<W> {
    pub fn new(err: W, cancel: CancellationToken) -> Self {
        Self {
            err,
            cancel,
            max_diagnostic_bytes: MAX_DIAGNOSTIC_LINE_BYTES,
        }
    }

    pub fn with_limits(err: W, cancel: CancellationToken, limits: TransportLimits) -> Self {
        Self {
            err,
            cancel,
            max_diagnostic_bytes: limits.max_diagnostic_bytes,
        }
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// One diagnostic line on stderr. Newlines are rejected, not rewritten.
    pub fn write_line(&mut self, message: &str) -> Result<(), StdioError> {
        self.cancel.check()?;
        if message.is_empty() {
            return Err(StdioError::EmptyDiagnostic);
        }
        if message.contains('\n') || message.contains('\r') {
            return Err(StdioError::DiagnosticNotSingleLine);
        }
        if message.len() > self.max_diagnostic_bytes {
            return Err(StdioError::FrameTooLarge {
                limit: self.max_diagnostic_bytes,
                observed: message.len(),
            });
        }
        self.err
            .write_all(message.as_bytes())
            .map_err(StdioError::Io)?;
        self.err.write_all(b"\n").map_err(StdioError::Io)?;
        self.err.flush().map_err(StdioError::Io)?;
        Ok(())
    }
}

impl<R: Read, W: Write> StdioTransport<R, W> {
    pub fn new(reader: R, writer: W, cancel: CancellationToken) -> Self {
        let limits = TransportLimits::default_limits();
        Self {
            reader: FrameReader {
                inner: reader,
                stash: Vec::new(),
                max_frame_bytes: limits.max_frame_bytes,
            },
            writer: FrameWriter {
                inner: writer,
                max_frame_bytes: limits.max_frame_bytes,
            },
            state: TransportState::Connected,
            cancel,
            limits,
        }
    }

    pub fn with_limits(
        reader: R,
        writer: W,
        cancel: CancellationToken,
        limits: TransportLimits,
    ) -> Result<Self, StdioError> {
        Ok(Self {
            reader: FrameReader::new(reader, limits.max_frame_bytes)?,
            writer: FrameWriter::new(writer, limits.max_frame_bytes)?,
            state: TransportState::Connected,
            cancel,
            limits,
        })
    }

    pub fn state(&self) -> TransportState {
        self.state
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, TransportState::Connected)
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn limits(&self) -> TransportLimits {
        self.limits
    }

    /// Mark the transport disconnected. Subsequent recv/send fail closed.
    pub fn disconnect(&mut self, reason: DisconnectReason) {
        if matches!(self.state, TransportState::Connected) {
            self.state = TransportState::Disconnected(reason);
        }
    }

    /// Read the next inbound frame. Recoverable faults emit a JSON-RPC error.
    pub fn recv(&mut self) -> Result<Incoming, StdioError> {
        self.ensure_connected()?;
        if self.cancel.is_cancelled() {
            self.disconnect(DisconnectReason::Cancelled);
            return Err(StdioError::Cancelled);
        }
        let frame = match self.reader.read_frame(&self.cancel) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.disconnect(DisconnectReason::PeerClosed);
                return Err(StdioError::Disconnected {
                    reason: DisconnectReason::PeerClosed,
                });
            }
            Err(StdioError::Cancelled) => {
                self.disconnect(DisconnectReason::Cancelled);
                return Err(StdioError::Cancelled);
            }
            Err(StdioError::UnrecoverableFrame { kind }) => {
                self.disconnect(DisconnectReason::UnrecoverableFrame);
                return Err(StdioError::UnrecoverableFrame { kind });
            }
            Err(StdioError::Io(err)) => {
                self.disconnect(DisconnectReason::Io);
                return Err(StdioError::Io(err));
            }
            Err(StdioError::FrameTooLarge { .. }) => {
                return self.reject(ProtocolFault::FrameTooLarge);
            }
            Err(err) => return Err(err),
        };
        if frame.len() > self.limits.max_frame_bytes {
            return self.reject(ProtocolFault::FrameTooLarge);
        }
        match decode_message(&frame) {
            Ok(message) => Ok(Incoming::Message(message)),
            Err(fault) => self.reject(fault),
        }
    }

    /// Write one outbound JSON-RPC frame to the exclusive protocol writer.
    pub fn send(&mut self, message: &JsonRpcMessage) -> Result<(), StdioError> {
        self.ensure_connected()?;
        if self.cancel.is_cancelled() {
            self.disconnect(DisconnectReason::Cancelled);
            return Err(StdioError::Cancelled);
        }
        match self.writer.write_message(message, &self.cancel) {
            Ok(()) => Ok(()),
            Err(StdioError::Cancelled) => {
                self.disconnect(DisconnectReason::Cancelled);
                Err(StdioError::Cancelled)
            }
            Err(StdioError::Io(err)) => {
                self.disconnect(DisconnectReason::Io);
                Err(StdioError::Io(err))
            }
            Err(err) => Err(err),
        }
    }

    pub fn into_inner(self) -> (R, W) {
        (self.reader.into_inner(), self.writer.into_inner())
    }

    fn reject(&mut self, fault: ProtocolFault) -> Result<Incoming, StdioError> {
        self.send(&JsonRpcMessage::protocol_error(fault))?;
        Ok(Incoming::ProtocolFault(fault))
    }

    fn ensure_connected(&self) -> Result<(), StdioError> {
        match self.state {
            TransportState::Connected => Ok(()),
            TransportState::Disconnected(reason) => Err(StdioError::Disconnected { reason }),
        }
    }
}

impl<R: Read, W: Write, E: Write> StdioSession<R, W, E> {
    pub fn new(reader: R, writer: W, err: E, cancel: CancellationToken) -> Self {
        let limits = TransportLimits::default_limits();
        Self {
            transport: StdioTransport::new(reader, writer, cancel.clone()),
            log: StderrLog::with_limits(err, cancel, limits),
        }
    }

    pub fn with_limits(
        reader: R,
        writer: W,
        err: E,
        cancel: CancellationToken,
        limits: TransportLimits,
    ) -> Result<Self, StdioError> {
        Ok(Self {
            transport: StdioTransport::with_limits(reader, writer, cancel.clone(), limits)?,
            log: StderrLog::with_limits(err, cancel, limits),
        })
    }

    pub fn transport(&mut self) -> &mut StdioTransport<R, W> {
        &mut self.transport
    }

    pub fn log(&mut self) -> &mut StderrLog<E> {
        &mut self.log
    }
}

impl StdioTransport<io::Stdin, io::Stdout> {
    /// Stdout is protocol-only. Diagnostics must use [`StderrLog`].
    pub fn stdio(cancel: CancellationToken) -> Self {
        Self::new(io::stdin(), io::stdout(), cancel)
    }
}

impl StdioSession<io::Stdin, io::Stdout, io::Stderr> {
    /// Pair exclusive stdout frames with a stderr diagnostic sink.
    pub fn stdio(cancel: CancellationToken) -> Self {
        Self::new(io::stdin(), io::stdout(), io::stderr(), cancel)
    }
}

fn encode_message(message: &JsonRpcMessage, max_frame_bytes: usize) -> Result<Vec<u8>, StdioError> {
    validate_outbound(message)?;
    let mut buf = serde_json::to_vec(message).map_err(StdioError::Encode)?;
    if buf.contains(&b'\n') {
        return Err(StdioError::EmbeddedNewline);
    }
    if buf.len() > max_frame_bytes {
        return Err(StdioError::FrameTooLarge {
            limit: max_frame_bytes,
            observed: buf.len(),
        });
    }
    buf.push(b'\n');
    Ok(buf)
}

fn decode_message(bytes: &[u8]) -> Result<JsonRpcMessage, ProtocolFault> {
    let text = std::str::from_utf8(bytes).map_err(|_| ProtocolFault::ParseError)?;
    let value: Value = serde_json::from_str(text).map_err(|_| ProtocolFault::ParseError)?;
    decode_value(value)
}

fn decode_value(value: Value) -> Result<JsonRpcMessage, ProtocolFault> {
    let obj = value.as_object().ok_or(ProtocolFault::InvalidRequest)?;
    match obj.get("jsonrpc") {
        Some(Value::String(version)) if version == JSONRPC_VERSION => {}
        _ => return Err(ProtocolFault::InvalidRequest),
    }
    let id = match obj.get("id") {
        None => None,
        Some(value) => Some(JsonRpcId::from_value(value)?),
    };
    let method = match obj.get("method") {
        None => None,
        Some(Value::String(method)) if !method.is_empty() => Some(method.clone()),
        Some(_) => return Err(ProtocolFault::InvalidRequest),
    };
    let params = match obj.get("params") {
        None => None,
        Some(value) if value.is_object() || value.is_array() => Some(value.clone()),
        Some(_) => return Err(ProtocolFault::InvalidRequest),
    };
    let result = obj.get("result").cloned();
    let error = match obj.get("error") {
        None => None,
        Some(value) => Some(JsonRpcErrorObject::from_value(value)?),
    };
    match (method, result, error, id) {
        (Some(method), None, None, Some(id)) => Ok(JsonRpcMessage::Request { id, method, params }),
        (Some(method), None, None, None) => Ok(JsonRpcMessage::Notification { method, params }),
        (None, Some(result), None, Some(id)) => Ok(JsonRpcMessage::Result { id, result }),
        (None, None, Some(error), Some(id)) => Ok(JsonRpcMessage::Error { id, error }),
        _ => Err(ProtocolFault::InvalidRequest),
    }
}

fn validate_outbound(message: &JsonRpcMessage) -> Result<(), StdioError> {
    match message {
        JsonRpcMessage::Request { method, params, .. }
        | JsonRpcMessage::Notification { method, params } => {
            if method.is_empty() {
                return Err(StdioError::InvalidOutbound);
            }
            if matches!(params, Some(p) if !p.is_object() && !p.is_array()) {
                return Err(StdioError::InvalidOutbound);
            }
            Ok(())
        }
        JsonRpcMessage::Error { error, .. } => {
            if error.message.is_empty() {
                Err(StdioError::InvalidOutbound)
            } else {
                Ok(())
            }
        }
        JsonRpcMessage::Result { .. } => Ok(()),
    }
}

fn take_complete_line(stash: &mut Vec<u8>) -> Option<Vec<u8>> {
    let idx = stash.iter().position(|&b| b == b'\n')?;
    let mut line: Vec<u8> = stash.drain(..=idx).collect();
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const GOLDEN_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
    const GOLDEN_NOTIFICATION: &str =
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1"}}"#;
    const GOLDEN_RESULT: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#;
    const GOLDEN_PARSE_ERROR: &str =
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#;

    fn limits(max_frame_bytes: usize) -> TransportLimits {
        TransportLimits::new(max_frame_bytes, 64).expect("limits")
    }

    fn transport(
        input: &[u8],
        limits: TransportLimits,
    ) -> StdioTransport<Cursor<Vec<u8>>, Vec<u8>> {
        StdioTransport::with_limits(
            Cursor::new(input.to_vec()),
            Vec::new(),
            CancellationToken::new(),
            limits,
        )
        .expect("transport")
    }

    fn frame_bytes(message: &JsonRpcMessage) -> Vec<u8> {
        let mut encoded = serde_json::to_vec(message).expect("encode");
        encoded.push(b'\n');
        encoded
    }

    fn stdout_lines(stdout: &[u8]) -> Vec<Value> {
        stdout
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let text = std::str::from_utf8(line).expect("utf8");
                assert!(!text.contains('\n'));
                serde_json::from_str::<Value>(text).expect("json-rpc frame")
            })
            .collect()
    }

    #[test]
    fn golden_jsonrpc_frames_round_trip() {
        let request = JsonRpcMessage::Request {
            id: JsonRpcId::Number(1),
            method: "initialize".into(),
            params: None,
        };
        let notification = JsonRpcMessage::Notification {
            method: "session/update".into(),
            params: Some(serde_json::json!({"sessionId": "s1"})),
        };
        let result = JsonRpcMessage::Result {
            id: JsonRpcId::Number(1),
            result: serde_json::json!({"protocolVersion": 1}),
        };
        let parse_error = JsonRpcMessage::protocol_error(ProtocolFault::ParseError);

        assert_eq!(
            serde_json::to_string(&request).expect("ser"),
            GOLDEN_REQUEST
        );
        assert_eq!(
            serde_json::to_string(&notification).expect("ser"),
            GOLDEN_NOTIFICATION
        );
        assert_eq!(serde_json::to_string(&result).expect("ser"), GOLDEN_RESULT);
        assert_eq!(
            serde_json::to_string(&parse_error).expect("ser"),
            GOLDEN_PARSE_ERROR
        );

        let decoded_request: JsonRpcMessage =
            serde_json::from_str(GOLDEN_REQUEST).expect("de request");
        let decoded_notification: JsonRpcMessage =
            serde_json::from_str(GOLDEN_NOTIFICATION).expect("de notification");
        let decoded_result: JsonRpcMessage =
            serde_json::from_str(GOLDEN_RESULT).expect("de result");
        let decoded_error: JsonRpcMessage =
            serde_json::from_str(GOLDEN_PARSE_ERROR).expect("de error");
        assert_eq!(decoded_request, request);
        assert_eq!(decoded_notification, notification);
        assert_eq!(decoded_result, result);
        assert_eq!(decoded_error, parse_error);
    }

    #[test]
    fn encoded_frames_have_no_embedded_newlines() {
        let message = JsonRpcMessage::Result {
            id: JsonRpcId::String("id\nbreak".into()),
            result: serde_json::json!({"text": "line1\nline2"}),
        };
        let encoded = message.encode().expect("encode");
        assert_eq!(encoded.last().copied(), Some(b'\n'));
        assert_eq!(encoded.iter().filter(|b| **b == b'\n').count(), 1);
        let decoded = JsonRpcMessage::decode(&encoded[..encoded.len() - 1]).expect("decode");
        assert_eq!(decoded, message);
    }

    #[test]
    fn recv_reads_pipelined_requests() {
        let mut input = GOLDEN_REQUEST.as_bytes().to_vec();
        input.push(b'\n');
        input.extend_from_slice(GOLDEN_NOTIFICATION.as_bytes());
        input.push(b'\n');
        let mut session = transport(&input, limits(256));
        match session.recv().expect("first") {
            Incoming::Message(JsonRpcMessage::Request { method, id, .. }) => {
                assert_eq!(method, "initialize");
                assert_eq!(id, JsonRpcId::Number(1));
            }
            other => panic!("unexpected first item: {other:?}"),
        }
        match session.recv().expect("second") {
            Incoming::Message(JsonRpcMessage::Notification { method, .. }) => {
                assert_eq!(method, "session/update");
            }
            other => panic!("unexpected second item: {other:?}"),
        }
        assert!(session.is_connected());
    }

    #[test]
    fn malformed_json_writes_parse_error_and_keeps_session() {
        let mut input = b"not-json\n".to_vec();
        input.extend_from_slice(GOLDEN_REQUEST.as_bytes());
        input.push(b'\n');
        let mut session = transport(&input, limits(256));
        assert_eq!(
            session.recv().expect("fault"),
            Incoming::ProtocolFault(ProtocolFault::ParseError)
        );
        assert!(session.is_connected());
        match session.recv().expect("recovered") {
            Incoming::Message(JsonRpcMessage::Request { method, .. }) => {
                assert_eq!(method, "initialize");
            }
            other => panic!("session did not recover: {other:?}"),
        }
        let (_, stdout) = session.into_inner();
        let lines = stdout_lines(&stdout);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(lines[0]["id"], Value::Null);
        assert!(!String::from_utf8_lossy(&stdout).contains("not-json"));
    }

    #[test]
    fn invalid_request_writes_protocol_error_without_kernel_involvement() {
        let mut session = transport(b"[1,2]\n", limits(256));
        assert_eq!(
            session.recv().expect("fault"),
            Incoming::ProtocolFault(ProtocolFault::InvalidRequest)
        );
        assert_eq!(session.state(), TransportState::Connected);
        let (_, stdout) = session.into_inner();
        let lines = stdout_lines(&stdout);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(lines[0]["error"]["code"], INVALID_REQUEST);
        assert_eq!(lines[0]["error"]["message"], INVALID_REQUEST_MESSAGE);
    }

    #[test]
    fn oversized_delimited_frame_is_a_recoverable_protocol_error() {
        let mut input = vec![b'x'; 200];
        input.push(b'\n');
        input.extend_from_slice(GOLDEN_REQUEST.as_bytes());
        input.push(b'\n');
        let mut session = transport(&input, limits(128));
        assert_eq!(
            session.recv().expect("fault"),
            Incoming::ProtocolFault(ProtocolFault::FrameTooLarge)
        );
        assert!(session.is_connected());
        match session.recv().expect("recovered") {
            Incoming::Message(JsonRpcMessage::Request { method, .. }) => {
                assert_eq!(method, "initialize");
            }
            other => panic!("session did not recover: {other:?}"),
        }
        let (_, stdout) = session.into_inner();
        let lines = stdout_lines(&stdout);
        assert_eq!(lines[0]["error"]["code"], INVALID_REQUEST);
        assert_eq!(lines[0]["error"]["message"], FRAME_TOO_LARGE_MESSAGE);
    }

    #[test]
    fn oversized_undelimited_frame_disconnects_without_panic() {
        let mut session = transport(&[b'x'; 32], limits(8));
        match session.recv() {
            Err(StdioError::UnrecoverableFrame {
                kind: FrameFault::TooLarge,
            }) => {}
            other => panic!("expected unrecoverable too-large, got {other:?}"),
        }
        assert_eq!(
            session.state(),
            TransportState::Disconnected(DisconnectReason::UnrecoverableFrame)
        );
        assert!(matches!(
            session.send(&JsonRpcMessage::protocol_error(ProtocolFault::ParseError)),
            Err(StdioError::Disconnected {
                reason: DisconnectReason::UnrecoverableFrame
            })
        ));
        let (_, stdout) = session.into_inner();
        assert!(stdout.is_empty());
    }

    #[test]
    fn incomplete_frame_at_eof_disconnects() {
        let mut session = transport(b"{\"jsonrpc\":\"2.0\"", limits(256));
        match session.recv() {
            Err(StdioError::UnrecoverableFrame {
                kind: FrameFault::Incomplete,
            }) => {}
            other => panic!("expected incomplete, got {other:?}"),
        }
        assert_eq!(
            session.state(),
            TransportState::Disconnected(DisconnectReason::UnrecoverableFrame)
        );
    }

    #[test]
    fn peer_close_is_an_explicit_disconnect() {
        let mut session = transport(b"", limits(256));
        match session.recv() {
            Err(StdioError::Disconnected {
                reason: DisconnectReason::PeerClosed,
            }) => {}
            other => panic!("expected peer closed, got {other:?}"),
        }
        assert_eq!(
            session.state(),
            TransportState::Disconnected(DisconnectReason::PeerClosed)
        );
        assert!(matches!(
            session.recv(),
            Err(StdioError::Disconnected {
                reason: DisconnectReason::PeerClosed
            })
        ));
    }

    #[test]
    fn explicit_disconnect_refuses_further_io() {
        let mut session = transport(GOLDEN_REQUEST.as_bytes(), limits(256));
        session.disconnect(DisconnectReason::Explicit);
        assert_eq!(
            session.state(),
            TransportState::Disconnected(DisconnectReason::Explicit)
        );
        assert!(matches!(
            session.recv(),
            Err(StdioError::Disconnected {
                reason: DisconnectReason::Explicit
            })
        ));
    }

    #[test]
    fn cancellation_disconnects_the_transport() {
        let cancel = CancellationToken::new();
        let mut session = StdioTransport::with_limits(
            Cursor::new(GOLDEN_REQUEST.as_bytes().to_vec()),
            Vec::new(),
            cancel.clone(),
            limits(256),
        )
        .expect("transport");
        cancel.cancel();
        assert!(matches!(session.recv(), Err(StdioError::Cancelled)));
        assert_eq!(
            session.state(),
            TransportState::Disconnected(DisconnectReason::Cancelled)
        );
    }

    #[test]
    fn diagnostics_never_touch_protocol_stdout() {
        let cancel = CancellationToken::new();
        let mut session = StdioSession::with_limits(
            Cursor::new(Vec::new()),
            Vec::new(),
            Vec::new(),
            cancel,
            limits(256),
        )
        .expect("session");
        session
            .transport()
            .send(&JsonRpcMessage::protocol_error(ProtocolFault::ParseError))
            .expect("send");
        session.log().write_line("acp ready").expect("log");
        let stdout = session.transport.writer.inner.clone();
        let stderr = session.log.err.clone();
        assert_eq!(stdout_lines(&stdout).len(), 1);
        assert_eq!(stderr, b"acp ready\n");
        assert!(!String::from_utf8_lossy(&stdout).contains("acp ready"));
    }

    #[test]
    fn crlf_and_blank_lines_are_accepted() {
        let mut input = b"\n\r\n".to_vec();
        input.extend_from_slice(GOLDEN_REQUEST.as_bytes());
        input.extend_from_slice(b"\r\n");
        let mut session = transport(&input, limits(256));
        match session.recv().expect("request") {
            Incoming::Message(JsonRpcMessage::Request { method, .. }) => {
                assert_eq!(method, "initialize");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn send_writes_only_jsonrpc_frames() {
        let mut session = transport(b"", limits(256));
        let message = JsonRpcMessage::Result {
            id: JsonRpcId::Number(7),
            result: serde_json::json!({"ok": true}),
        };
        session.send(&message).expect("send");
        let (_, stdout) = session.into_inner();
        assert_eq!(stdout, frame_bytes(&message));
        assert_eq!(stdout_lines(&stdout).len(), 1);
    }

    #[test]
    fn outbound_invalid_message_is_not_written() {
        let mut session = transport(b"", limits(256));
        let invalid = JsonRpcMessage::Notification {
            method: String::new(),
            params: None,
        };
        assert!(matches!(
            session.send(&invalid),
            Err(StdioError::InvalidOutbound)
        ));
        assert!(session.is_connected());
        let (_, stdout) = session.into_inner();
        assert!(stdout.is_empty());
    }
}
