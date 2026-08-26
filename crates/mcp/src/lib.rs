#![forbid(unsafe_code)]

pub mod catalog;
pub mod gateway;
pub mod server;
pub mod transport;
pub mod trust;

pub use catalog::{
    BoundedDescription, CATALOG_SCHEMA, CATALOG_SCHEMA_VERSION, CapabilityHint, CatalogChange,
    CatalogError, CatalogIngestOutcome, CatalogItem, CatalogItemId, CatalogKind, CatalogMeta,
    CatalogTrust, ExternalSource, ExternalToolId, ListKinds, ListPage, MAX_CURSOR_BYTES,
    MAX_DESCRIPTION_BYTES, MAX_DESCRIPTOR_BYTES, MAX_IDENT_BYTES, MAX_ITEMS_PER_KIND,
    MAX_LIST_PAGES, MAX_SERVERS, MAX_URI_BYTES, McpCatalogCache, McpServerId, ServerCatalog,
    ServerListResults, encode_list_request, parse_list_page,
};
pub use gateway::{
    EvidenceCollector, ExternalCall, ExternalCallRequest, GATEWAY_SCHEMA, GATEWAY_SCHEMA_VERSION,
    GatewayActor, GatewayError, InvocationContext, InvocationEvidence, MAX_ARGUMENT_FIELDS,
    MAX_ARGUMENTS_BYTES, MAX_RESULT_BYTES, MAX_RESULT_CONTENT_ITEMS, MAX_RESULT_TEXT_BYTES,
    McpCallResult, McpGateway, McpMiddleware, MiddlewareStage, ResultTrustLabel,
    encode_tools_call, parse_tools_result,
};
pub use server::{
    AuthorizedInvocation, AuthorizedResourceRead, MAX_CALL_ARGUMENT_FIELDS,
    MAX_CALL_ARGUMENTS_BYTES, MAX_CALL_RESULT_BYTES, MAX_PUBLISHED_CAPABILITIES,
    MAX_PUBLISHED_RESOURCES, MAX_PUBLISHED_TOOLS, McpServer, McpServerConfig, PrivilegedClass,
    PublishedExecutor, PublishedResource, PublishedSurface, RapidLmTool, SERVER_SCHEMA,
    SERVER_SCHEMA_VERSION, ServerClient, ServerError, ServerResourceContents, ServerToolResult,
};
pub use transport::{
    ClientCapabilities, DEFAULT_IO_TIMEOUT, HttpAuthScope, HttpConnectRequest, HttpRequest,
    HttpResponse, ImplementationInfo, IoBounds, LoopbackTransport, MAX_FRAME_BYTES,
    MAX_IMPLEMENTATION_NAME_BYTES, MAX_IMPLEMENTATION_VERSION_BYTES, MAX_IO_TIMEOUT,
    MAX_MCP_SESSION_ID_BYTES, MCP_PRIOR_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION, McpSession,
    McpSessionId, McpTransport, NegotiatedHandshake, ProtocolVersion, SECRET_TARGET_PREFIX,
    ServerCapabilities, StdioTransport, StreamableHttpIo, StreamableHttpTransport, TransportError,
    TransportKind,
};
pub use trust::{
    EndpointFingerprint, MAX_ALLOWED_CAPABILITIES, MAX_ALLOWED_TOOLS, MAX_FINGERPRINT_BYTES,
    MAX_TRUST_CATALOG_BYTES, MAX_TRUST_RECORDS, McpServerIdentity, McpTrustRecord, McpTrustStatus,
    McpTrustStore, ProjectCredential, ProjectMcpServerSpec, ServerOrigin, TRUST_SCHEMA,
    TRUST_SCHEMA_VERSION, TrustCatalogView, TrustError, TrustGrant,
};
