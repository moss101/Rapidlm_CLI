//! ACP v1/v2 interoperability fixtures.
//!
//! Covers negotiated versions, mapped event/error frames, and unknown-field
//! behavior. Client handshake text is untrusted data (T-007): unknown
//! capabilities are ignored and never treated as grants.

#![forbid(unsafe_code)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use acp::v1::{
    encode_permission_request, encode_prompt_response, encode_session_update, map_kernel_event,
};
use acp::{
    negotiate, AcpVersion, CancellationToken, HandleResult, InitializeOutcome, JsonRpcId,
    JsonRpcMessage, MappedEvent, StopReason, V1Adapter, V2Adapter, INTERNAL_ERROR, INVALID_PARAMS,
    METHOD_NOT_FOUND, PROTOCOL_VERSION, V2_PROTOCOL_VERSION,
};
use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
use kernel::InProcessKernelClient;
use protocol::{EventId, ProjectId, RedactionClass, SessionId, TraceId};
use serde_json::{json, Value};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

const GOLDEN_UUID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
const SECRET_CANARY: &str = "hunter2-capability-lease";

const GOLDEN_V1_INITIALIZE: &str = concat!(
    r#"{"protocolVersion":1,"agentCapabilities":{"#,
    r#""loadSession":true,"promptCapabilities":{"#,
    r#""image":false,"audio":false,"embeddedContext":false},"#,
    r#""mcpCapabilities":{"http":false,"sse":false}},"#,
    r#""agentInfo":{"name":"rapidlm","version":"0.1.0"},"authMethods":[]}"#
);

const GOLDEN_V2_INITIALIZE: &str = concat!(
    r#"{"protocolVersion":2,"capabilities":{"session":{}},"#,
    r#""info":{"name":"rapidlm","title":"RapidLM","version":"0.1.0"},"#,
    r#""authMethods":[]}"#
);

const GOLDEN_TOOL_CALL: &str = concat!(
    r#"{"jsonrpc":"2.0","method":"session/update","params":{"#,
    r#""sessionId":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","update":{"#,
    r#""kind":"search","sessionUpdate":"tool_call","status":"pending","#,
    r#""title":"repo.search","toolCallId":"call_7"}}}"#
);

const GOLDEN_TOOL_COMPLETED: &str = concat!(
    r#"{"jsonrpc":"2.0","method":"session/update","params":{"#,
    r#""sessionId":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","update":{"#,
    r#""sessionUpdate":"tool_call_update","status":"completed","title":"repo.search","toolCallId":"call_7"}}}"#
);

const GOLDEN_PERMISSION: &str = concat!(
    r#"{"jsonrpc":"2.0","id":2,"method":"session/request_permission","params":{"#,
    r#""options":["#,
    r#"{"kind":"allow_once","name":"Allow once","optionId":"allow-once"},"#,
    r#"{"kind":"allow_always","name":"Allow always","optionId":"allow-always"},"#,
    r#"{"kind":"reject_once","name":"Reject once","optionId":"reject-once"},"#,
    r#"{"kind":"reject_always","name":"Reject always","optionId":"reject-always"}],"#,
    r#""sessionId":"018f3c8a-7e2b-7a10-8c4d-0123456789ab","toolCall":{"#,
    r#""title":"repo.search","toolCallId":"call_7"}}}"#
);

const GOLDEN_PROMPT_STOP: &str = r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#;

const GOLDEN_UNKNOWN_METHOD: &str =
    r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"Method not found"}}"#;

struct TempClient {
    path: PathBuf,
    client: InProcessKernelClient,
}

impl TempClient {
    fn create() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rapidlm-acp-compat-{}-{seq}.sqlite",
            std::process::id()
        ));
        remove_db_files(&path);
        let client = InProcessKernelClient::open(&path).expect("open client");
        Self { path, client }
    }
}

impl Drop for TempClient {
    fn drop(&mut self) {
        remove_db_files(&self.path);
    }
}

fn remove_db_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar(path, "-wal"));
    let _ = std::fs::remove_file(sidecar(path, "-shm"));
    let _ = std::fs::remove_file(sidecar(path, "-journal"));
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("in-process kernel future stayed pending"),
    }
}

fn actor() -> ActorRef {
    ActorRef::new(ActorKind::ExternalHuman, GOLDEN_UUID).expect("actor")
}

fn session_id() -> SessionId {
    SessionId::from_str(GOLDEN_UUID).expect("session")
}

fn encode(message: &JsonRpcMessage) -> String {
    let mut bytes = message.encode().expect("encode");
    assert_eq!(bytes.pop(), Some(b'\n'));
    String::from_utf8(bytes).expect("utf8")
}

fn envelope(kind: EventKind, payload: Value) -> EventEnvelope<Value> {
    EventEnvelope::new(
        EventId::from_str(GOLDEN_UUID).expect("event"),
        session_id(),
        2,
        RecordedAt::from_str("2026-08-14T15:20:04.123Z").expect("ts"),
        ActorRef::new(ActorKind::Agent, GOLDEN_UUID).expect("actor"),
        TraceId::from_str(GOLDEN_UUID).expect("trace"),
        kind,
        RedactionClass::Project,
        payload,
    )
}

fn assert_no_canary(value: &impl std::fmt::Debug) {
    let rendered = format!("{value:?}");
    assert!(
        !rendered.contains(SECRET_CANARY),
        "leaked canary: {rendered}"
    );
}

#[test]
fn negotiate_covers_v1_and_v2_versions() {
    let v1 = negotiate(&json!({"protocolVersion": 1})).expect("v1");
    assert_eq!(v1.protocol(), AcpVersion::V1);
    assert_eq!(v1.protocol().as_u32(), PROTOCOL_VERSION);
    assert!(!v1.agent_capabilities().session());
    assert!(!v1.client_capabilities().elicitation_form());

    let v2 = negotiate(&json!({
        "protocolVersion": 2,
        "info": {"name": "editor", "version": "1.0.0"}
    }))
    .expect("v2");
    assert_eq!(v2.protocol(), AcpVersion::V2);
    assert_eq!(v2.protocol().as_u32(), V2_PROTOCOL_VERSION);
    assert!(v2.agent_capabilities().session());
    assert!(!v2.agent_capabilities().prompt_image());
    assert!(!v2.agent_capabilities().mcp_http());
    assert!(!v2.agent_capabilities().auth());
}

#[test]
fn unknown_v2_fields_are_ignored_not_assumed() {
    let handshake = negotiate(&json!({
        "protocolVersion": 2,
        "info": {"name": "editor", "version": "1.0.0"},
        "capabilities": {
            "fs": {"readTextFile": true, "writeTextFile": true},
            "terminal": true,
            "sudo": {"grant_capability": SECRET_CANARY},
            "elicitation": {"form": {}, "unknownMode": {}, "_custom": {}},
            "_zed.dev": {"workspace": true}
        },
        "_meta": {"secret": SECRET_CANARY}
    }))
    .expect("unknown fields ignored");
    assert_eq!(handshake.protocol(), AcpVersion::V2);
    assert!(handshake.client_capabilities().elicitation_form());
    assert!(!handshake.client_capabilities().elicitation_url());
    assert!(!handshake.agent_capabilities().prompt_image());
    assert!(!handshake.agent_capabilities().session_delete());
    assert!(!handshake.agent_capabilities().additional_directories());
    assert!(!handshake.agent_capabilities().auth());
    assert_no_canary(&handshake);
}

#[test]
fn malformed_known_capability_and_zero_version_fail_closed() {
    let zero = negotiate(&json!({"protocolVersion": 0})).expect_err("zero");
    assert_eq!(zero.jsonrpc_code(), INVALID_PARAMS);
    assert_eq!(zero.to_string(), "Invalid params");

    let shape = negotiate(&json!({
        "protocolVersion": 2,
        "info": {"name": "editor", "version": "1.0.0"},
        "capabilities": {"elicitation": true}
    }))
    .expect_err("boolean marker");
    assert_eq!(shape.jsonrpc_code(), INVALID_PARAMS);
    assert!(!shape.to_string().contains(SECRET_CANARY));
}

#[test]
fn v1_and_v2_initialize_goldens_match_wire() {
    let tmp = TempClient::create();
    let mut v1 = V1Adapter::new(
        tmp.client.clone(),
        ProjectId::new(),
        actor(),
        CancellationToken::new(),
    );
    let v1_result = block_on(v1.initialize(json!({
        "protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": true}, "terminal": true},
        "clientInfo": {"name": "editor", "version": "1.0.0"},
        "unknownClientField": SECRET_CANARY
    })))
    .expect("v1 init");
    assert_eq!(v1_result.protocol_version(), PROTOCOL_VERSION);
    assert_eq!(
        serde_json::to_string(&v1_result).expect("ser"),
        GOLDEN_V1_INITIALIZE
    );
    assert_no_canary(&v1_result);

    let mut v2 = V2Adapter::new(
        tmp.client.clone(),
        ProjectId::new(),
        actor(),
        CancellationToken::new(),
    );
    let outcome = block_on(v2.initialize(json!({
        "protocolVersion": 2,
        "info": {"name": "editor", "version": "1.0.0"},
        "capabilities": {
            "fs": {"readTextFile": true},
            "elicitation": {"form": {}, "url": {}}
        }
    })))
    .expect("v2 init");
    assert_eq!(outcome.protocol_version(), V2_PROTOCOL_VERSION);
    let json = match &outcome {
        InitializeOutcome::V2(result) => serde_json::to_string(result).expect("ser"),
        other => panic!("expected v2, got {other:?}"),
    };
    assert_eq!(json, GOLDEN_V2_INITIALIZE);
    assert!(tmp.path.exists());
}

#[test]
fn event_stream_and_error_goldens() {
    let tool_payload = json!({
        "call_id": "call_7",
        "tool": "repo.search",
        "kind": "search",
        "grant_capability": SECRET_CANARY
    });

    let requested = map_kernel_event(&envelope(EventKind::ToolRequested, tool_payload.clone()))
        .expect("requested");
    let MappedEvent::SessionUpdate(update) = &requested else {
        panic!("expected session update, got {requested:?}");
    };
    let frame = encode_session_update(update).expect("encode tool");
    assert_eq!(encode(&frame), GOLDEN_TOOL_CALL);
    assert_no_canary(&frame);

    let completed = map_kernel_event(&envelope(EventKind::ToolCompleted, tool_payload.clone()))
        .expect("completed");
    let MappedEvent::SessionUpdate(update) = &completed else {
        panic!("expected tool update, got {completed:?}");
    };
    let frame = encode_session_update(update).expect("encode completed");
    assert_eq!(encode(&frame), GOLDEN_TOOL_COMPLETED);

    let permission = map_kernel_event(&envelope(EventKind::ApprovalRequested, tool_payload))
        .expect("permission");
    let MappedEvent::PermissionRequired(request) = &permission else {
        panic!("expected permission, got {permission:?}");
    };
    let frame = encode_permission_request(JsonRpcId::Number(2), request).expect("encode perm");
    assert_eq!(encode(&frame), GOLDEN_PERMISSION);
    assert_no_canary(&frame);

    let stopped = map_kernel_event(&envelope(
        EventKind::TurnCompleted,
        json!({"reason": "end_turn"}),
    ))
    .expect("stopped");
    assert!(matches!(
        stopped,
        MappedEvent::PromptStopped(StopReason::EndTurn)
    ));
    let frame = encode_prompt_response(JsonRpcId::Number(3), StopReason::EndTurn);
    assert_eq!(encode(&frame), GOLDEN_PROMPT_STOP);

    assert!(map_kernel_event(&envelope(EventKind::SessionCreated, json!({}))).is_none());
}

#[test]
fn unknown_method_and_cancelled_adapter_fail_closed() {
    let tmp = TempClient::create();
    let mut acp = V2Adapter::new(
        tmp.client.clone(),
        ProjectId::new(),
        actor(),
        CancellationToken::new(),
    );
    block_on(acp.initialize(json!({
        "protocolVersion": 2,
        "info": {"name": "editor", "version": "1.0.0"}
    })))
    .expect("init");
    let result = block_on(acp.handle(&JsonRpcMessage::Request {
        id: JsonRpcId::Number(4),
        method: "session/set_mode".into(),
        params: Some(json!({"sessionId": GOLDEN_UUID, "secret": SECRET_CANARY})),
    }))
    .expect("handle");
    match result {
        HandleResult::Reply(message) => {
            assert_eq!(encode(&message), GOLDEN_UNKNOWN_METHOD);
            assert_no_canary(&message);
            match message {
                JsonRpcMessage::Error { error, .. } => {
                    assert_eq!(error.code(), METHOD_NOT_FOUND);
                }
                other => panic!("expected error, got {other:?}"),
            }
        }
        other => panic!("expected reply, got {other:?}"),
    }

    let cancel = CancellationToken::new();
    let mut cancelled = V2Adapter::new(
        tmp.client.clone(),
        ProjectId::new(),
        actor(),
        cancel.clone(),
    );
    cancel.cancel();
    let err = block_on(cancelled.initialize(json!({
        "protocolVersion": 2,
        "info": {"name": "editor", "version": "1.0.0"}
    })))
    .expect_err("cancelled");
    assert_eq!(err.jsonrpc_code(), INTERNAL_ERROR);
    assert_eq!(err.to_string(), "acp adapter cancelled");
    assert!(cancelled.handshake().is_none());
}
