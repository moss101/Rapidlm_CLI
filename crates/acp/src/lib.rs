#![forbid(unsafe_code)]

pub mod stdio;
pub mod v1;
pub mod v2;

pub use stdio::{
    CancellationToken, DisconnectReason, FrameFault, FrameReader, FrameWriter, INTERNAL_ERROR,
    INVALID_PARAMS, INVALID_REQUEST, Incoming, JSONRPC_VERSION, JsonRpcErrorObject, JsonRpcId,
    JsonRpcMessage, MAX_DIAGNOSTIC_LINE_BYTES, MAX_FRAME_BYTES, METHOD_NOT_FOUND, PARSE_ERROR,
    ProtocolFault, StderrLog, StdioError, StdioSession, StdioTransport, TransportLimits,
    TransportState,
};
pub use v1::{
    ContentBlock, FileDiff, HandleResult, InitializeResult, MappedEvent, NewSessionResult,
    MAX_DIFF_BYTES, MAX_DIFF_PATH_BYTES, PROTOCOL_VERSION, PermissionOutcome, PermissionRequest,
    PromptTurn, SessionUpdate, SessionUpdateNotification, StopReason, ToolCallStatus, V1Adapter,
    V1Error,
};
pub use v2::{
    AcpVersion, AgentCapabilities, ClientCapabilities, InitializeOutcome, NegotiatedHandshake,
    PROTOCOL_VERSION as V2_PROTOCOL_VERSION, V2Adapter, V2Error, V2InitializeResult, negotiate,
};
