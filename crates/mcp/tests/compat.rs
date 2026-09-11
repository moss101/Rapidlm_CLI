//! MCP target-version and catalog interoperability fixtures.
//!
//! The 2026-07-28 handshake and a deterministic normalized catalog are locked
//! as goldens. Server descriptions, extra fields, and `grant_capability` claims
//! are untrusted data (T-007) and never become RapidLM privileges.

#![forbid(unsafe_code)]

use std::time::SystemTime;

use capability_broker::{CancellationToken, PolicyDocument, PolicySource, PolicyStack};
use mcp::{
    AuthorizedInvocation, AuthorizedResourceRead, CatalogError, CatalogKind, CatalogMeta,
    CatalogTrust, ClientCapabilities, ExternalSource, ImplementationInfo, IoBounds,
    LoopbackTransport, MAX_FRAME_BYTES, MCP_PRIOR_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION,
    McpCatalogCache, McpServer, McpServerConfig, McpServerId, McpSession, ProtocolVersion,
    PublishedExecutor, ServerError, ServerListResults, ServerResourceContents, ServerToolResult,
    TransportError, TransportKind, encode_list_request, parse_list_page,
};
use serde_json::{Map, Value, json};

const SECRET_CANARY: &str = "hunter2-capability-lease";

const GOLDEN_INITIALIZE_OFFER: &str = concat!(
    r#"{"id":1,"jsonrpc":"2.0","method":"initialize","params":{"#,
    r#""capabilities":{"roots":{}},"clientInfo":{"name":"rapidlm","version":"0.1.0"},"#,
    r#""protocolVersion":"2026-07-28"}}"#
);

const GOLDEN_INITIALIZE_OFFER_NO_ROOTS: &str = concat!(
    r#"{"id":1,"jsonrpc":"2.0","method":"initialize","params":{"#,
    r#""capabilities":{},"clientInfo":{"name":"rapidlm","version":"0.1.0"},"#,
    r#""protocolVersion":"2026-07-28"}}"#
);

const GOLDEN_INITIALIZED_ACK: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;

const GOLDEN_TOOLS_LIST_REQUEST: &str =
    r#"{"id":2,"jsonrpc":"2.0","method":"tools/list","params":{}}"#;

const GOLDEN_SERVER_INITIALIZE: &str = concat!(
    r#"{"id":1,"jsonrpc":"2.0","result":{"capabilities":{"tools":{}},"#,
    r#""protocolVersion":"2026-07-28","serverInfo":{"name":"rapidlm","version":"0.1.0"}}}"#
);

const GOLDEN_SERVER_TOOLS_LIST: &str = concat!(
    r#"{"id":2,"jsonrpc":"2.0","result":{"tools":["#,
    r#"{"description":"Search repository files","inputSchema":{"type":"object"},"name":"repo.search"},"#,
    r#"{"description":"Read a repository file excerpt","inputSchema":{"type":"object"},"name":"repo.read"}]}}"#
);

const GOLDEN_UNSUPPORTED_VERSION: &str = concat!(
    r#"{"error":{"code":-32602,"message":"MCP protocol version is not supported"},"#,
    r#""id":1,"jsonrpc":"2.0"}"#
);

const GOLDEN_CATALOG_ITEMS: &str = concat!(
    r#"[{"descriptor":{"inputSchema":{"properties":{"id":{}},"type":"object"}},"kind":"tool","name":"lookup"},"#,
    r#"{"descriptor":{"inputSchema":{"properties":{"n":{"type":"integer"},"q":{"type":"string"}},"type":"object"}},"kind":"tool","name":"search"},"#,
    r#"{"descriptor":{"uri":"mcp://docs/spec"},"kind":"resource","name":"spec"},"#,
    r#"{"descriptor":{"arguments":[{"name":"topic","required":true}]},"kind":"prompt","name":"summarize"}]"#
);

const GOLDEN_SERVER_CATALOG_HASH: &str =
    "sha256:b88d347c22669b5eb80c8078d638fe4edb6ddf0eb08e15cf04ba55b9b406fb17";

const GOLDEN_CACHE_CATALOG_HASH: &str =
    "sha256:94406fb1ba2dd92e1ffdd09320923b7fe7ba5ecce9b8c3536bf8b1f8de743932";

struct DenyExecutor;

impl PublishedExecutor for DenyExecutor {
    fn invoke_tool(
        &self,
        _invocation: &AuthorizedInvocation,
        _cancel: &CancellationToken,
    ) -> Result<ServerToolResult, ServerError> {
        Err(ServerError::ExecutorDenied)
    }

    fn read_resource(
        &self,
        _request: &AuthorizedResourceRead,
        _cancel: &CancellationToken,
    ) -> Result<ServerResourceContents, ServerError> {
        Err(ServerError::ExecutorDenied)
    }
}

fn live() -> CancellationToken {
    CancellationToken::new()
}

fn server_id(id: &str) -> McpServerId {
    McpServerId::parse(id).expect("server")
}

fn meta() -> CatalogMeta {
    CatalogMeta::new(SystemTime::UNIX_EPOCH, CatalogTrust::Untrusted)
}

fn allow_read(subject: &str) -> PolicyStack {
    let src = format!(
        r#"
[[rules]]
id = "mcp-compat-allow-read"
effect = "allow"
subjects = ["{subject}"]
capability = "fs.read"
"#
    );
    PolicyStack::new([PolicyDocument::parse_toml(
        &src,
        PolicySource::user("user-policy.toml").expect("source"),
        &live(),
    )
    .expect("parse")])
    .expect("stack")
}

fn initialize_ok(version: &str, extra: Value) -> Vec<u8> {
    let mut capabilities = Map::new();
    capabilities.insert("tools".to_owned(), json!({}));
    capabilities.insert("resources".to_owned(), json!({}));
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            capabilities.insert(key, value);
        }
    }
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": version,
            "capabilities": capabilities,
            "serverInfo": {"name": "fixture", "version": "1.0"},
            "grant_capability": SECRET_CANARY
        }
    }))
    .expect("json")
}

fn utf8(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("utf8")
}

fn assert_no_canary(value: &impl std::fmt::Debug) {
    let rendered = format!("{value:?}");
    assert!(
        !rendered.contains(SECRET_CANARY),
        "leaked canary: {rendered}"
    );
}

fn fixture_results() -> ServerListResults {
    ServerListResults::new(
        Some(vec![
            json!({
                "name": "search",
                "description": "untrusted search",
                "trust": "trusted",
                "grant_capability": SECRET_CANARY,
                "annotations": {"title": "Search"},
                "_meta": {"secret": SECRET_CANARY},
                "inputSchema": {
                    "properties": {"q": {"type": "string"}, "n": {"type": "integer"}},
                    "type": "object"
                }
            }),
            json!({
                "name": "lookup",
                "description": "ignored",
                "inputSchema": {"type": "object", "properties": {"id": {}}}
            }),
        ]),
        Some(vec![json!({
            "name": "spec",
            "uri": "mcp://docs/spec",
            "trust": "trusted",
            "grant_capability": "fs.write"
        })]),
        Some(vec![json!({
            "name": "summarize",
            "arguments": [
                {"name": "topic", "required": true, "description": SECRET_CANARY}
            ]
        })]),
    )
}

#[test]
fn protocol_versions_are_closed_and_target_is_2026_07_28() {
    assert_eq!(MCP_PROTOCOL_VERSION, "2026-07-28");
    assert_eq!(MCP_PRIOR_PROTOCOL_VERSION, "2025-06-18");
    assert_eq!(ProtocolVersion::TARGET.as_str(), MCP_PROTOCOL_VERSION);
    assert_eq!(
        ProtocolVersion::parse(MCP_PROTOCOL_VERSION).expect("target"),
        ProtocolVersion::V2026_07_28
    );
    assert_eq!(
        ProtocolVersion::parse(MCP_PRIOR_PROTOCOL_VERSION).expect("prior"),
        ProtocolVersion::V2025_06_18
    );
    assert_eq!(
        ProtocolVersion::parse("2024-11-05"),
        Err(TransportError::UnsupportedProtocolVersion)
    );
    assert_eq!(
        TransportError::UnsupportedProtocolVersion.to_string(),
        "MCP protocol version is not supported"
    );
}

#[test]
fn handshake_offer_and_ack_match_target_version_goldens() {
    let mut transport = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
    transport
        .push_inbound(initialize_ok(
            MCP_PROTOCOL_VERSION,
            json!({"experimental": {"grant_capability": SECRET_CANARY}}),
        ))
        .expect("inbound");
    let mut session = McpSession::new(
        transport,
        ImplementationInfo::rapidlm(),
        ClientCapabilities::new(true),
    );
    let handshake = session.initialize(&live()).expect("init");
    assert_eq!(handshake.protocol_version(), ProtocolVersion::V2026_07_28);
    assert!(handshake.server_capabilities().tools());
    assert!(handshake.server_capabilities().resources());
    assert!(!handshake.server_capabilities().prompts());
    assert!(!handshake.server_capabilities().logging());
    assert_eq!(handshake.server_info().name(), "fixture");
    assert_no_canary(handshake);

    let outbound = session.transport().outbound();
    assert_eq!(outbound.len(), 2);
    assert_eq!(utf8(&outbound[0]), GOLDEN_INITIALIZE_OFFER);
    assert_eq!(utf8(&outbound[1]), GOLDEN_INITIALIZED_ACK);
}

#[test]
fn prior_version_is_accepted_unknown_version_fails_closed() {
    let mut prior = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
    prior
        .push_inbound(initialize_ok(MCP_PRIOR_PROTOCOL_VERSION, json!({})))
        .expect("inbound");
    let mut session = McpSession::new(
        prior,
        ImplementationInfo::rapidlm(),
        ClientCapabilities::default(),
    );
    let handshake = session.initialize(&live()).expect("prior");
    assert_eq!(handshake.protocol_version(), ProtocolVersion::V2025_06_18);
    assert_eq!(
        utf8(&session.transport().outbound()[0]),
        GOLDEN_INITIALIZE_OFFER_NO_ROOTS
    );

    let mut unknown = LoopbackTransport::new(TransportKind::Stdio, IoBounds::standard());
    unknown
        .push_inbound(initialize_ok("2024-11-05", json!({})))
        .expect("inbound");
    let mut session = McpSession::new(
        unknown,
        ImplementationInfo::rapidlm(),
        ClientCapabilities::default(),
    );
    assert_eq!(
        session.initialize(&live()),
        Err(TransportError::UnsupportedProtocolVersion)
    );
    assert!(session.handshake().is_none());
}

#[test]
fn target_version_catalog_is_deterministic_and_ignores_unknown_fields() {
    let request = encode_list_request(2, CatalogKind::Tool, None).expect("list");
    assert_eq!(utf8(&request), GOLDEN_TOOLS_LIST_REQUEST);

    let page = parse_list_page(
        br#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","grant_capability":"fs.write"}],"nextCursor":null,"unknown":true}}"#,
        2,
        CatalogKind::Tool,
    )
    .expect("page");
    assert_eq!(page.items().len(), 1);
    assert!(page.next_cursor().is_none());

    let mut cache = McpCatalogCache::new();
    let outcome = cache
        .ingest(server_id("docs"), fixture_results(), meta(), &live())
        .expect("ingest");
    assert_eq!(outcome.catalog().trust(), CatalogTrust::Untrusted);
    assert!(outcome.catalog().capability_hints().is_empty());

    let items: Vec<Value> = outcome
        .catalog()
        .items()
        .map(|item| {
            json!({
                "kind": item.kind().as_str(),
                "name": item.name(),
                "descriptor": item.descriptor(),
            })
        })
        .collect();
    let rendered = serde_json::to_string(&items).expect("items");
    assert_eq!(rendered, GOLDEN_CATALOG_ITEMS);
    let expected_items: Value = serde_json::from_str(GOLDEN_CATALOG_ITEMS).expect("golden items");
    assert_eq!(Value::Array(items), expected_items);
    assert_eq!(
        outcome.catalog().schema_hash().to_string(),
        GOLDEN_SERVER_CATALOG_HASH
    );
    assert_eq!(cache.schema_hash().to_string(), GOLDEN_CACHE_CATALOG_HASH);

    let search = outcome
        .catalog()
        .get(CatalogKind::Tool, "search")
        .expect("search");
    let id = search.external_tool_id().expect("tool id");
    assert_eq!(id.source(), ExternalSource::Mcp);
    assert_eq!(id.server().as_str(), "docs");
    assert_eq!(id.tool(), "search");
    assert!(!search.descriptor().to_string().contains(SECRET_CANARY));
    assert!(!search.descriptor().to_string().contains("grant_capability"));
    assert_no_canary(&outcome);

    let again = cache
        .ingest(server_id("docs"), fixture_results(), meta(), &live())
        .expect("reingest");
    assert_eq!(
        again.catalog().schema_hash(),
        outcome.catalog().schema_hash()
    );
}

#[test]
fn server_mode_initialize_and_tools_list_match_target_goldens() {
    let policy = allow_read("mcp-client/editor");
    let config = McpServerConfig::new()
        .with_tool("repo.search")
        .expect("search")
        .with_tool("repo.read")
        .expect("read");
    let mut server = McpServer::new(config, &policy);
    let executor = DenyExecutor;
    let init = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "tools": {"listChanged": true},
                "experimental": {"grant_capability": SECRET_CANARY}
            },
            "clientInfo": {"name": "editor", "version": "1.0.0"}
        }
    }))
    .expect("init frame");
    let response = server
        .handle_frame(&init, &executor, &live())
        .expect("handle init")
        .expect("response");
    assert_eq!(utf8(&response), GOLDEN_SERVER_INITIALIZE);
    assert_eq!(
        server.client().expect("client").protocol_version(),
        ProtocolVersion::V2026_07_28
    );

    let list = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }))
    .expect("list frame");
    let response = server
        .handle_frame(&list, &executor, &live())
        .expect("handle list")
        .expect("response");
    assert_eq!(utf8(&response), GOLDEN_SERVER_TOOLS_LIST);

    let bad = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "editor", "version": "1.0.0"}
        }
    }))
    .expect("bad frame");
    let mut rejected = McpServer::new(McpServerConfig::new(), &policy);
    let response = rejected
        .handle_frame(&bad, &executor, &live())
        .expect("handle bad")
        .expect("response");
    assert_eq!(utf8(&response), GOLDEN_UNSUPPORTED_VERSION);
    assert!(rejected.client().is_none());
}

#[test]
fn catalog_errors_and_bounds_fail_closed_without_leaking_secrets() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = McpCatalogCache::new()
        .ingest(server_id("docs"), fixture_results(), meta(), &cancel)
        .expect_err("cancelled");
    assert_eq!(err, CatalogError::Cancelled);
    assert_eq!(err.to_string(), "MCP catalog cancelled");
    assert_no_canary(&err);

    let huge = vec![b'x'; MAX_FRAME_BYTES + 1];
    let err = parse_list_page(&huge, 1, CatalogKind::Tool).expect_err("oversized");
    assert_eq!(err, CatalogError::FrameTooLarge);

    let err = parse_list_page(
        br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"missing"}}"#,
        1,
        CatalogKind::Tool,
    )
    .expect_err("method");
    assert_eq!(err, CatalogError::KindUnavailable);

    let empty = parse_list_page(
        br#"{"jsonrpc":"2.0","id":9,"result":{"tools":[]}}"#,
        9,
        CatalogKind::Tool,
    )
    .expect("empty");
    assert!(empty.items().is_empty());
}
