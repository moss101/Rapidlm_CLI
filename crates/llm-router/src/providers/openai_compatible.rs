//! OpenAI-compatible chat/responses streaming adapter.
//!
//! Transport conversion only: compiled messages/tools are mapped to the
//! profile's endpoint style. API-key plaintext is attached by
//! [`WireAuthorization`] at the HTTP boundary and is never stored on
//! model/config objects. Provider error bodies are discarded after
//! classification (threats `T-012`, `T-001`).

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::credentials::{CredentialResolver, EphemeralCredential, ProviderProfile};
use crate::provider::{
    CancellationToken, CanonicalMessage, CanonicalModelRequest, CanonicalToolSpec, ContentPart,
    FinishReason, MAX_STREAM_DELTA_BYTES, MAX_STREAM_EVENTS, MessageRole, ModelStream,
    ModelStreamEvent, NormalizedUsage, ProviderAdapter, ProviderCapabilities, ProviderError,
    ToolCallId, ToolName, UsageCost, UsageExtValue,
};

/// Wire schema name for [`OpenAiCompatibleConfig`].
pub const OPENAI_COMPATIBLE_CONFIG_SCHEMA: &str = "rapidlm.openai_compatible_config";

/// v1 schema version for OpenAI-compatible adapter config.
pub const OPENAI_COMPATIBLE_SCHEMA_VERSION: u16 = 1;

/// Default connect/read/write timeout for the HTTP/1.1 transport.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum accepted HTTP request body (encoded provider payload).
pub const MAX_HTTP_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// Maximum accepted HTTP response body (SSE or JSON error).
pub const MAX_HTTP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum UTF-8 bytes for a configured base URL.
pub const MAX_BASE_URL_BYTES: usize = 512;

/// Maximum HTTP header lines accepted on a response.
pub const MAX_RESPONSE_HEADERS: usize = 64;

/// Maximum UTF-8 bytes for one HTTP header line.
pub const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// How often to honor cancellation while parsing stream events.
const CANCEL_CHECK_EVERY: usize = 16;

/// Provider API surface selected by the profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum OpenAiApiStyle {
    ChatCompletions,
    Responses,
}

/// Handle-only profile plus the chat/responses endpoint it addresses.
#[derive(Clone, Eq, PartialEq)]
pub struct OpenAiCompatibleConfig {
    profile: ProviderProfile,
    endpoint: OpenAiCompatibleEndpoint,
    capabilities: ProviderCapabilities,
}

/// Validated origin + prefix for an OpenAI-compatible server.
#[derive(Clone, Eq, PartialEq)]
pub struct OpenAiCompatibleEndpoint {
    base_url: String,
    style: OpenAiApiStyle,
}

/// Outbound HTTP request built by the adapter. No `Authorization` header.
pub struct ProviderHttpRequest<'a> {
    url: &'a str,
    headers: &'a [(String, String)],
    body: &'a [u8],
    credential: &'a EphemeralCredential,
}

/// Bounded HTTP response. Debug omits the body (may contain provider secrets).
pub struct ProviderHttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Injectable HTTP. Implementations attach auth from [`WireAuthorization`].
pub trait HttpTransport: Send + Sync {
    fn execute(
        &self,
        request: &ProviderHttpRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<ProviderHttpResponse, ProviderError>;
}

/// Materializes `Authorization` at the wire only. Must not log the bytes.
pub trait WireAuthorization: Send + Sync {
    fn bearer_token(
        &self,
        credential: &EphemeralCredential,
        cancel: &CancellationToken,
    ) -> Result<String, ProviderError>;
}

/// Test/eval bearer that never reads [`EphemeralCredential`] plaintext.
pub struct StaticWireAuth {
    token: String,
}

/// Blocking HTTP/1.1 client. HTTPS/TLS is rejected (no silent cleartext downgrade).
pub struct Http1Transport<A> {
    auth: A,
    timeout: Duration,
    max_response_bytes: usize,
}

/// Streaming adapter for one provider profile.
pub struct OpenAiCompatibleAdapter<'store, T> {
    config: OpenAiCompatibleConfig,
    transport: T,
    store: &'store dyn auth::CredentialStore,
}

impl OpenAiApiStyle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }

    pub const fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/chat/completions",
            Self::Responses => "/responses",
        }
    }
}

impl OpenAiCompatibleEndpoint {
    pub fn new(base_url: impl AsRef<str>, style: OpenAiApiStyle) -> Result<Self, ProviderError> {
        let parsed = parse_http_url(base_url.as_ref())?;
        if host_is_blocked(&parsed.host) {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            base_url: parsed.normalized_base(),
            style,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub const fn style(&self) -> OpenAiApiStyle {
        self.style
    }

    pub fn request_url(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.style.path()
        )
    }
}

impl OpenAiCompatibleConfig {
    pub fn new(
        profile: ProviderProfile,
        endpoint: OpenAiCompatibleEndpoint,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ProviderError> {
        if !capabilities.streaming() {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            profile,
            endpoint,
            capabilities,
        })
    }

    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    pub fn endpoint(&self) -> &OpenAiCompatibleEndpoint {
        &self.endpoint
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
}

impl<'a> ProviderHttpRequest<'a> {
    /// Build an outbound request. Callers must not put `Authorization` in `headers`.
    pub fn new(
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
        credential: &'a EphemeralCredential,
    ) -> Self {
        Self {
            url,
            headers,
            body,
            credential,
        }
    }

    pub fn url(&self) -> &'a str {
        self.url
    }

    pub fn headers(&self) -> &'a [(String, String)] {
        self.headers
    }

    pub fn body(&self) -> &'a [u8] {
        self.body
    }

    pub fn credential(&self) -> &'a EphemeralCredential {
        self.credential
    }
}

impl ProviderHttpResponse {
    pub fn new(
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Self, ProviderError> {
        if headers.len() > MAX_RESPONSE_HEADERS || body.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        Ok(Self {
            status,
            headers,
            body,
        })
    }

    pub const fn status(&self) -> u16 {
        self.status
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case(name) {
                Some(v.as_str())
            } else {
                None
            }
        })
    }
}

impl StaticWireAuth {
    pub fn bearer(token: impl Into<String>) -> Result<Self, ProviderError> {
        let token = token.into();
        if token.is_empty() || token.len() > 8 * 1024 {
            return Err(ProviderError::InvalidRequest);
        }
        if token
            .bytes()
            .any(|b| b < 0x20 || b == 0x7f || b == b'\n' || b == b'\r')
        {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self { token })
    }
}

impl WireAuthorization for StaticWireAuth {
    fn bearer_token(
        &self,
        credential: &EphemeralCredential,
        cancel: &CancellationToken,
    ) -> Result<String, ProviderError> {
        cancel.check()?;
        if credential.byte_len() == 0 {
            return Err(ProviderError::AuthFailed);
        }
        Ok(self.token.clone())
    }
}

impl<A: WireAuthorization> Http1Transport<A> {
    pub fn new(auth: A) -> Self {
        Self {
            auth,
            timeout: DEFAULT_HTTP_TIMEOUT,
            max_response_bytes: MAX_HTTP_RESPONSE_BYTES,
        }
    }

    pub fn with_limits(auth: A, timeout: Duration, max_response_bytes: usize) -> Self {
        Self {
            auth,
            timeout,
            max_response_bytes,
        }
    }
}

impl<A: WireAuthorization> HttpTransport for Http1Transport<A> {
    fn execute(
        &self,
        request: &ProviderHttpRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<ProviderHttpResponse, ProviderError> {
        cancel.check()?;
        if request.body.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        if request
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        {
            return Err(ProviderError::InvalidRequest);
        }

        let parsed = parse_http_url(request.url)?;
        if parsed.scheme != UrlScheme::Http {
            return Err(ProviderError::InvalidRequest);
        }
        if host_is_blocked(&parsed.host) {
            return Err(ProviderError::InvalidRequest);
        }

        let addrs = (parsed.host.as_str(), parsed.port)
            .to_socket_addrs()
            .map_err(|_| ProviderError::Transient)?;
        let mut selected = None;
        for addr in addrs {
            cancel.check()?;
            if ip_is_blocked(addr.ip()) {
                return Err(ProviderError::InvalidRequest);
            }
            if selected.is_none() {
                selected = Some(addr);
            }
        }
        let addr = selected.ok_or(ProviderError::Transient)?;

        let token = self.auth.bearer_token(request.credential, cancel)?;
        if token
            .bytes()
            .any(|b| b < 0x20 || b == 0x7f || b == b'\n' || b == b'\r')
        {
            return Err(ProviderError::InvalidRequest);
        }

        cancel.check()?;
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)
            .map_err(|_| ProviderError::Transient)?;
        stream
            .set_read_timeout(Some(slice_timeout(self.timeout)))
            .map_err(|_| ProviderError::Transient)?;
        stream
            .set_write_timeout(Some(slice_timeout(self.timeout)))
            .map_err(|_| ProviderError::Transient)?;
        stream
            .set_nodelay(true)
            .map_err(|_| ProviderError::Transient)?;

        let deadline = Instant::now() + self.timeout;
        write_http_request(
            &mut stream,
            &parsed,
            request.headers,
            request.body,
            &token,
            cancel,
            deadline,
        )?;
        drop(token);

        read_http_response(&mut stream, self.max_response_bytes, cancel, deadline)
    }
}

impl<T: HttpTransport + ?Sized> HttpTransport for Box<T> {
    fn execute(
        &self,
        request: &ProviderHttpRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<ProviderHttpResponse, ProviderError> {
        (**self).execute(request, cancel)
    }
}

impl<'store, T: HttpTransport> OpenAiCompatibleAdapter<'store, T> {
    pub fn new(
        config: OpenAiCompatibleConfig,
        transport: T,
        store: &'store dyn auth::CredentialStore,
    ) -> Self {
        Self {
            config,
            transport,
            store,
        }
    }

    pub fn config(&self) -> &OpenAiCompatibleConfig {
        &self.config
    }

    pub fn invoke_sync(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, ProviderError> {
        cancel.check()?;
        validate_request(&req, &self.config)?;
        let resolver = CredentialResolver::new(self.store);
        let credential = resolver.resolve(
            self.config.profile.provider(),
            self.config.profile(),
            cancel,
        )?;
        let body = encode_provider_payload(&req, self.config.endpoint.style, cancel)?;
        let encoded = serde_json::to_vec(&body).map_err(|_| ProviderError::InvalidRequest)?;
        if encoded.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(ProviderError::BoundExceeded);
        }

        let url = self.config.endpoint.request_url();
        let request_id = req.request_id().as_str().to_owned();
        let headers = [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("x-rapidlm-request-id".to_owned(), request_id),
        ];
        cancel.check()?;
        let outbound = ProviderHttpRequest {
            url: &url,
            headers: &headers,
            body: &encoded,
            credential: &credential,
        };
        let response = self.transport.execute(&outbound, cancel)?;
        drop(credential);
        classify_http_error(&response)?;
        let events = parse_provider_stream(self.config.endpoint.style, &response.body, cancel)?;
        ModelStream::from_events(
            req.request_id().clone(),
            req.model().clone(),
            events,
            cancel,
        )
    }
}

impl<T: HttpTransport> ProviderAdapter for OpenAiCompatibleAdapter<'_, T> {
    fn capabilities(&self) -> ProviderCapabilities {
        self.config.capabilities.clone()
    }

    fn invoke(
        &self,
        req: CanonicalModelRequest,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        let result = self.invoke_sync(req, &cancel);
        async move { result }
    }
}

/// Convert a canonical request to the provider JSON object (no secrets).
pub fn encode_provider_payload(
    req: &CanonicalModelRequest,
    style: OpenAiApiStyle,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    cancel.check()?;
    match style {
        OpenAiApiStyle::ChatCompletions => encode_chat_completions(req, cancel),
        OpenAiApiStyle::Responses => encode_responses(req, cancel),
    }
}

fn validate_request(
    req: &CanonicalModelRequest,
    config: &OpenAiCompatibleConfig,
) -> Result<(), ProviderError> {
    if req.model().provider() != config.profile.provider() {
        return Err(ProviderError::InvalidRequest);
    }
    if req.messages().is_empty() {
        return Err(ProviderError::InvalidRequest);
    }
    if !req.tools().is_empty() && !config.capabilities.tools() {
        return Err(ProviderError::InvalidRequest);
    }
    if has_image_part(req) && !config.capabilities.vision() {
        return Err(ProviderError::InvalidRequest);
    }
    if !config.capabilities.streaming() {
        return Err(ProviderError::InvalidRequest);
    }
    Ok(())
}

fn has_image_part(req: &CanonicalModelRequest) -> bool {
    req.messages().iter().any(|message| {
        message
            .parts()
            .iter()
            .any(|part| matches!(part, ContentPart::Image { .. }))
    })
}

fn encode_chat_completions(
    req: &CanonicalModelRequest,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    let mut payload = Map::new();
    payload.insert(
        "model".to_owned(),
        Value::String(req.model().model().as_str().to_owned()),
    );
    payload.insert("stream".to_owned(), Value::Bool(true));
    let mut stream_options = Map::new();
    stream_options.insert("include_usage".to_owned(), Value::Bool(true));
    payload.insert("stream_options".to_owned(), Value::Object(stream_options));
    if let Some(max_output) = req.max_output_tokens() {
        payload.insert("max_tokens".to_owned(), Value::from(max_output));
    }

    let mut messages = Vec::with_capacity(req.messages().len());
    for (i, message) in req.messages().iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check()?;
        }
        messages.push(encode_chat_message(message)?);
    }
    payload.insert("messages".to_owned(), Value::Array(messages));

    if !req.tools().is_empty() {
        let mut tools = Vec::with_capacity(req.tools().len());
        for tool in req.tools() {
            tools.push(encode_chat_tool(tool));
        }
        payload.insert("tools".to_owned(), Value::Array(tools));
    }
    Ok(Value::Object(payload))
}

fn encode_responses(
    req: &CanonicalModelRequest,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    let mut payload = Map::new();
    payload.insert(
        "model".to_owned(),
        Value::String(req.model().model().as_str().to_owned()),
    );
    payload.insert("stream".to_owned(), Value::Bool(true));
    if let Some(max_output) = req.max_output_tokens() {
        payload.insert("max_output_tokens".to_owned(), Value::from(max_output));
    }

    let mut input = Vec::with_capacity(req.messages().len());
    for (i, message) in req.messages().iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check()?;
        }
        input.extend(encode_responses_items(message)?);
    }
    payload.insert("input".to_owned(), Value::Array(input));

    if !req.tools().is_empty() {
        let mut tools = Vec::with_capacity(req.tools().len());
        for tool in req.tools() {
            tools.push(encode_responses_tool(tool));
        }
        payload.insert("tools".to_owned(), Value::Array(tools));
    }
    Ok(Value::Object(payload))
}

fn encode_chat_message(message: &CanonicalMessage) -> Result<Value, ProviderError> {
    let mut object = Map::new();
    object.insert(
        "role".to_owned(),
        Value::String(message.role().as_str().to_owned()),
    );
    if let Some(call_id) = message.tool_call_id() {
        object.insert(
            "tool_call_id".to_owned(),
            Value::String(call_id.as_str().to_owned()),
        );
    }
    if !message.tool_calls().is_empty() {
        let calls = message
            .tool_calls()
            .iter()
            .map(|call| {
                let mut function = Map::new();
                function.insert(
                    "name".to_owned(),
                    Value::String(call.name().as_str().to_owned()),
                );
                function.insert(
                    "arguments".to_owned(),
                    Value::String(call.arguments().to_owned()),
                );
                let mut item = Map::new();
                item.insert(
                    "id".to_owned(),
                    Value::String(call.call_id().as_str().to_owned()),
                );
                item.insert("type".to_owned(), Value::String("function".to_owned()));
                item.insert("function".to_owned(), Value::Object(function));
                Value::Object(item)
            })
            .collect();
        object.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    object.insert("content".to_owned(), encode_chat_content(message.parts())?);
    Ok(Value::Object(object))
}

fn encode_chat_content(parts: &[ContentPart]) -> Result<Value, ProviderError> {
    if parts.is_empty() {
        return Ok(Value::Null);
    }
    if parts.len() == 1
        && let ContentPart::Text { text } = &parts[0]
    {
        return Ok(Value::String(text.clone()));
    }
    let mut encoded = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                encoded.push(serde_json::json!({"type": "text", "text": text}));
            }
            ContentPart::Image { artifact } => {
                encoded.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": artifact_url(&artifact.id)}
                }));
            }
        }
    }
    Ok(Value::Array(encoded))
}

fn encode_responses_items(message: &CanonicalMessage) -> Result<Vec<Value>, ProviderError> {
    if message.role() == MessageRole::Tool {
        let call_id = message
            .tool_call_id()
            .ok_or(ProviderError::InvalidRequest)?;
        let output = flatten_text_parts(message.parts());
        return Ok(vec![serde_json::json!({
            "type": "function_call_output",
            "call_id": call_id.as_str(),
            "output": output,
        })]);
    }
    if message.role() == MessageRole::Assistant && !message.tool_calls().is_empty() {
        let mut items = Vec::new();
        let text = flatten_text_parts(message.parts());
        if !text.is_empty() {
            items.push(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            }));
        }
        for call in message.tool_calls() {
            items.push(serde_json::json!({
                "type": "function_call",
                "call_id": call.call_id().as_str(),
                "name": call.name().as_str(),
                "arguments": call.arguments(),
            }));
        }
        return Ok(items);
    }

    let mut object = Map::new();
    object.insert("type".to_owned(), Value::String("message".to_owned()));
    object.insert(
        "role".to_owned(),
        Value::String(message.role().as_str().to_owned()),
    );
    object.insert(
        "content".to_owned(),
        encode_responses_content(message.role(), message.parts())?,
    );
    Ok(vec![Value::Object(object)])
}

fn encode_responses_content(
    role: MessageRole,
    parts: &[ContentPart],
) -> Result<Value, ProviderError> {
    let text_type = if role == MessageRole::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    let mut encoded = Vec::with_capacity(parts.len().max(1));
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                encoded.push(serde_json::json!({"type": text_type, "text": text}));
            }
            ContentPart::Image { artifact } => {
                encoded.push(serde_json::json!({
                    "type": "input_image",
                    "image_url": artifact_url(&artifact.id),
                }));
            }
        }
    }
    Ok(Value::Array(encoded))
}

fn encode_chat_tool(tool: &CanonicalToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.name().as_str(),
            "description": tool.description(),
            "parameters": tool.parameters(),
        }
    })
}

fn encode_responses_tool(tool: &CanonicalToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "name": tool.name().as_str(),
        "description": tool.description(),
        "parameters": tool.parameters(),
    })
}

fn flatten_text_parts(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text { text } = part {
            out.push_str(text);
        }
    }
    out
}

fn artifact_url(id: &protocol::ArtifactId) -> String {
    format!("artifact:{id}")
}

fn classify_http_error(response: &ProviderHttpResponse) -> Result<(), ProviderError> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    let parsed = parse_json_object(&response.body);
    match response.status {
        401 | 403 => Err(ProviderError::AuthFailed),
        429 => Err(ProviderError::RateLimited {
            retry_after_ms: response
                .header("retry-after")
                .and_then(parse_retry_after_ms),
        }),
        400 | 413 if parsed.as_ref().is_some_and(json_is_context_too_large) => {
            Err(ProviderError::ContextTooLarge)
        }
        408 | 409 | 425 | 500 | 502 | 503 | 504 => Err(ProviderError::Transient),
        400..=499 => Err(ProviderError::Permanent),
        _ => Err(ProviderError::Transient),
    }
}

fn parse_json_object(body: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(body).ok()?;
    let value: Value = serde_json::from_str(text).ok()?;
    value.is_object().then_some(value)
}

fn json_is_context_too_large(value: &Value) -> bool {
    let error = value.get("error").unwrap_or(value);
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let haystack = format!("{code} {kind} {message}").to_ascii_lowercase();
    haystack.contains("context_length_exceeded")
        || haystack.contains("context length")
        || haystack.contains("maximum context")
        || haystack.contains("too many tokens")
        || haystack.contains("prompt is too long")
        || haystack.contains("token limit")
}

fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.len() > 16 {
        return None;
    }
    let seconds: u64 = raw.parse().ok()?;
    seconds.checked_mul(1000)
}

fn parse_provider_stream(
    style: OpenAiApiStyle,
    body: &[u8],
    cancel: &CancellationToken,
) -> Result<Vec<ModelStreamEvent>, ProviderError> {
    cancel.check()?;
    let text = std::str::from_utf8(body).map_err(|_| ProviderError::Permanent)?;
    let mut events = Vec::new();
    let mut tool_ids: BTreeMap<u32, ToolCallId> = BTreeMap::new();
    let mut response_tools: BTreeMap<String, ToolCallId> = BTreeMap::new();
    let mut finish = None;
    let mut usage = NormalizedUsage::new(None, None, None, None, None, None, UsageCost::Unknown);
    let mut saw_data = false;
    let mut saw_done = false;

    for (i, block) in sse_data_blocks(text).into_iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check()?;
        }
        if block == "[DONE]" {
            saw_done = true;
            break;
        }
        let value: Value = serde_json::from_str(&block).map_err(|_| ProviderError::Permanent)?;
        if let Some(error) = value.get("error") {
            let _ = error;
            return map_in_stream_error(&value);
        }
        saw_data = true;
        match style {
            OpenAiApiStyle::ChatCompletions => {
                ingest_chat_chunk(
                    &value,
                    &mut events,
                    &mut tool_ids,
                    &mut finish,
                    &mut usage,
                    cancel,
                )?;
            }
            OpenAiApiStyle::Responses => {
                ingest_responses_event(
                    &value,
                    &mut events,
                    &mut response_tools,
                    &mut finish,
                    &mut usage,
                    cancel,
                )?;
            }
        }
        if events.len() > MAX_STREAM_EVENTS {
            return Err(ProviderError::BoundExceeded);
        }
    }

    if !saw_data {
        if let Some(value) = parse_json_object(body) {
            if value.get("error").is_some() {
                return map_in_stream_error(&value);
            }
            ingest_non_stream_completion(
                style,
                &value,
                &mut events,
                &mut finish,
                &mut usage,
                cancel,
            )?;
        } else {
            return Err(ProviderError::Permanent);
        }
    } else if finish.is_none() && !saw_done {
        // Mid-stream events without a terminal marker are a truncated prefix.
        return Err(ProviderError::Permanent);
    }

    let finish = finish.unwrap_or({
        if events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::ToolCallStart { .. }
                    | ModelStreamEvent::ToolCallArgumentsDelta { .. }
            )
        }) {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        }
    });
    push_event(&mut events, ModelStreamEvent::Completed { finish, usage })?;
    Ok(events)
}

fn map_in_stream_error(value: &Value) -> Result<Vec<ModelStreamEvent>, ProviderError> {
    if json_is_context_too_large(value) {
        return Err(ProviderError::ContextTooLarge);
    }
    let error = value.get("error").unwrap_or(value);
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
    let joined = format!("{code} {kind}").to_ascii_lowercase();
    if joined.contains("auth") || joined.contains("invalid_api_key") {
        return Err(ProviderError::AuthFailed);
    }
    if joined.contains("rate") {
        return Err(ProviderError::RateLimited {
            retry_after_ms: None,
        });
    }
    Err(ProviderError::Permanent)
}

fn ingest_chat_chunk(
    value: &Value,
    events: &mut Vec<ModelStreamEvent>,
    tool_ids: &mut BTreeMap<u32, ToolCallId>,
    finish: &mut Option<FinishReason>,
    usage: &mut NormalizedUsage,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    cancel.check()?;
    if let Some(raw_usage) = value.get("usage").filter(|item| !item.is_null()) {
        *usage = normalize_openai_usage(raw_usage)?;
        push_event(events, ModelStreamEvent::Usage(usage.clone()))?;
    }
    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return Ok(());
    };
    for choice in choices {
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            *finish = Some(map_finish_reason(reason)?);
        }
        let delta = choice
            .get("delta")
            .unwrap_or(choice.get("message").unwrap_or(&Value::Null));
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            push_text_deltas(events, text)?;
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            ingest_chat_tool_deltas(calls, events, tool_ids)?;
        }
    }
    Ok(())
}

fn ingest_chat_tool_deltas(
    calls: &[Value],
    events: &mut Vec<ModelStreamEvent>,
    tool_ids: &mut BTreeMap<u32, ToolCallId>,
) -> Result<(), ProviderError> {
    for call in calls {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
        if let Some(id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            let call_id = ToolCallId::parse(id)?;
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(ToolName::parse)
                .transpose()?
                .or_else(|| {
                    call.get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map(ToolName::parse)
                        .transpose()
                        .ok()
                        .flatten()
                });
            if let Some(name) = name {
                push_event(
                    events,
                    ModelStreamEvent::ToolCallStart {
                        call_id: call_id.clone(),
                        name,
                    },
                )?;
            }
            tool_ids.insert(index, call_id);
        }
        let arguments = call
            .get("function")
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .or_else(|| call.get("arguments").and_then(Value::as_str))
            .unwrap_or("");
        if !arguments.is_empty() {
            let call_id = tool_ids
                .get(&index)
                .cloned()
                .ok_or(ProviderError::Permanent)?;
            push_argument_deltas(events, call_id, arguments)?;
        }
    }
    Ok(())
}

fn ingest_responses_event(
    value: &Value,
    events: &mut Vec<ModelStreamEvent>,
    tools: &mut BTreeMap<String, ToolCallId>,
    finish: &mut Option<FinishReason>,
    usage: &mut NormalizedUsage,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    cancel.check()?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("response.completed");

    if let Some(raw_usage) = value
        .pointer("/response/usage")
        .or_else(|| value.get("usage"))
        .filter(|item| !item.is_null())
    {
        *usage = normalize_openai_usage(raw_usage)?;
        if event_type.ends_with("completed") || value.get("usage").is_some() {
            push_event(events, ModelStreamEvent::Usage(usage.clone()))?;
        }
    }

    match event_type {
        "response.output_text.delta" | "response.text.delta" => {
            if let Some(text) = value
                .get("delta")
                .and_then(Value::as_str)
                .or_else(|| value.get("text").and_then(Value::as_str))
            {
                push_text_deltas(events, text)?;
            }
        }
        "response.output_item.added" => {
            if let Some(item) = value.get("item") {
                ingest_responses_item(item, events, tools)?;
            }
        }
        "response.function_call_arguments.delta" => {
            let index_key = value.get("output_index").map(ToString::to_string);
            let key = value
                .get("item_id")
                .and_then(Value::as_str)
                .or(index_key.as_deref())
                .unwrap_or("");
            let call_id = tools.get(key).cloned().ok_or(ProviderError::Permanent)?;
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                push_argument_deltas(events, call_id, delta)?;
            }
        }
        "response.completed" | "response.incomplete" => {
            *finish = Some(if event_type.ends_with("incomplete") {
                FinishReason::Length
            } else if events.iter().any(|event| {
                matches!(
                    event,
                    ModelStreamEvent::ToolCallStart { .. }
                        | ModelStreamEvent::ToolCallArgumentsDelta { .. }
                )
            }) {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            });
        }
        "response.failed" => return Err(ProviderError::Permanent),
        _ => {
            if let Some(text) = value.pointer("/delta").and_then(Value::as_str) {
                push_text_deltas(events, text)?;
            }
        }
    }
    Ok(())
}

fn ingest_responses_item(
    item: &Value,
    events: &mut Vec<ModelStreamEvent>,
    tools: &mut BTreeMap<String, ToolCallId>,
) -> Result<(), ProviderError> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    if kind != "function_call" && kind != "tool_call" {
        return Ok(());
    }
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .ok_or(ProviderError::Permanent)?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ProviderError::Permanent)?;
    let parsed_id = ToolCallId::parse(call_id)?;
    let parsed_name = ToolName::parse(name)?;
    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
        tools.insert(item_id.to_owned(), parsed_id.clone());
    }
    tools.insert(call_id.to_owned(), parsed_id.clone());
    push_event(
        events,
        ModelStreamEvent::ToolCallStart {
            call_id: parsed_id.clone(),
            name: parsed_name,
        },
    )?;
    if let Some(arguments) = item.get("arguments").and_then(Value::as_str)
        && !arguments.is_empty()
    {
        push_argument_deltas(events, parsed_id, arguments)?;
    }
    Ok(())
}

fn ingest_non_stream_completion(
    style: OpenAiApiStyle,
    value: &Value,
    events: &mut Vec<ModelStreamEvent>,
    finish: &mut Option<FinishReason>,
    usage: &mut NormalizedUsage,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    match style {
        OpenAiApiStyle::ChatCompletions => {
            ingest_chat_chunk(value, events, &mut BTreeMap::new(), finish, usage, cancel)
        }
        OpenAiApiStyle::Responses => {
            ingest_responses_event(value, events, &mut BTreeMap::new(), finish, usage, cancel)
        }
    }
}

fn normalize_openai_usage(value: &Value) -> Result<NormalizedUsage, ProviderError> {
    let object = value.as_object().ok_or(ProviderError::InvalidRequest)?;
    let input = first_u64(object, &["input_tokens", "prompt_tokens"])?;
    let output = first_u64(object, &["output_tokens", "completion_tokens"])?;
    let cached = object
        .get("cached_input_tokens")
        .map(json_u64)
        .transpose()?
        .flatten()
        .or(object
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .map(json_u64)
            .transpose()?
            .flatten())
        .or(object
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .map(json_u64)
            .transpose()?
            .flatten());
    let uncached = match (input, cached) {
        (Some(input), Some(cached)) if input >= cached => Some(input - cached),
        _ => first_u64(object, &["uncached_input_tokens"])?,
    };
    let reasoning = object
        .get("reasoning_tokens")
        .map(json_u64)
        .transpose()?
        .flatten()
        .or(object
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .map(json_u64)
            .transpose()?
            .flatten())
        .or(object
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .map(json_u64)
            .transpose()?
            .flatten());
    let tool = first_u64(object, &["tool_tokens"])?;
    let cost = if let Some(micros) = first_u64(object, &["cost_usd_micros"])? {
        UsageCost::Reported { usd_micros: micros }
    } else {
        UsageCost::Unknown
    };
    let mut usage = NormalizedUsage::new(input, cached, uncached, output, reasoning, tool, cost);
    if let Some(total) = first_u64(object, &["total_tokens"])? {
        usage.insert_extra("total_tokens", UsageExtValue::U64(total))?;
    }
    Ok(usage)
}

fn first_u64(object: &Map<String, Value>, keys: &[&str]) -> Result<Option<u64>, ProviderError> {
    for key in keys {
        if let Some(value) = object.get(*key) {
            return json_u64(value);
        }
    }
    Ok(None)
}

fn json_u64(value: &Value) -> Result<Option<u64>, ProviderError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(number) => number
            .as_u64()
            .ok_or(ProviderError::InvalidRequest)
            .map(Some),
        _ => Err(ProviderError::InvalidRequest),
    }
}

fn map_finish_reason(raw: &str) -> Result<FinishReason, ProviderError> {
    match raw {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "tool_calls" | "function_call" => Ok(FinishReason::ToolCalls),
        "cancelled" => Ok(FinishReason::Cancelled),
        "content_filter" => Err(ProviderError::Permanent),
        "" | "null" => Ok(FinishReason::Stop),
        _ => Err(ProviderError::Permanent),
    }
}

fn push_text_deltas(events: &mut Vec<ModelStreamEvent>, text: &str) -> Result<(), ProviderError> {
    for chunk in bounded_chunks(text)? {
        push_event(events, ModelStreamEvent::TextDelta { text: chunk })?;
    }
    Ok(())
}

fn push_argument_deltas(
    events: &mut Vec<ModelStreamEvent>,
    call_id: ToolCallId,
    arguments: &str,
) -> Result<(), ProviderError> {
    let chunks = bounded_chunks(arguments)?;
    for chunk in chunks {
        push_event(
            events,
            ModelStreamEvent::ToolCallArgumentsDelta {
                call_id: call_id.clone(),
                arguments_delta: chunk,
            },
        )?;
    }
    Ok(())
}

fn push_event(
    events: &mut Vec<ModelStreamEvent>,
    event: ModelStreamEvent,
) -> Result<(), ProviderError> {
    if events.len() >= MAX_STREAM_EVENTS {
        return Err(ProviderError::BoundExceeded);
    }
    events.push(event);
    Ok(())
}

fn bounded_chunks(text: &str) -> Result<Vec<String>, ProviderError> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_STREAM_DELTA_BYTES);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            return Err(ProviderError::Permanent);
        }
        chunks.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    Ok(chunks)
}

fn sse_data_blocks(body: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(data);
        } else if line.is_empty() && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UrlScheme {
    Http,
    Https,
}

struct ParsedUrl {
    scheme: UrlScheme,
    host: String,
    port: u16,
    path: String,
}

impl ParsedUrl {
    fn normalized_base(&self) -> String {
        let scheme = match self.scheme {
            UrlScheme::Http => "http",
            UrlScheme::Https => "https",
        };
        let path = if self.path == "/" {
            String::new()
        } else {
            self.path.trim_end_matches('/').to_owned()
        };
        format!("{scheme}://{}:{}{path}", self.host, self.port)
    }
}

fn parse_http_url(raw: &str) -> Result<ParsedUrl, ProviderError> {
    if raw.len() > MAX_BASE_URL_BYTES {
        return Err(ProviderError::BoundExceeded);
    }
    if raw.is_empty()
        || raw.contains('@')
        || raw.contains('#')
        || raw.contains('\\')
        || raw.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        return Err(ProviderError::InvalidRequest);
    }
    let (scheme, rest) = if let Some(rest) = raw.strip_prefix("https://") {
        (UrlScheme::Https, rest)
    } else if let Some(rest) = raw.strip_prefix("http://") {
        (UrlScheme::Http, rest)
    } else {
        return Err(ProviderError::InvalidRequest);
    };
    if rest.is_empty() || rest.contains('@') {
        return Err(ProviderError::InvalidRequest);
    }
    let (hostport, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if hostport.is_empty() || hostport.contains('?') || path.contains('?') {
        return Err(ProviderError::InvalidRequest);
    }
    if hostport.starts_with('[') {
        return Err(ProviderError::InvalidRequest);
    }
    let (host, port) = if let Some(index) = hostport.rfind(':') {
        let host = &hostport[..index];
        let port = hostport[index + 1..]
            .parse::<u16>()
            .map_err(|_| ProviderError::InvalidRequest)?;
        if port == 0 {
            return Err(ProviderError::InvalidRequest);
        }
        (host, port)
    } else {
        let port = match scheme {
            UrlScheme::Http => 80,
            UrlScheme::Https => 443,
        };
        (hostport, port)
    };
    if !is_valid_host(host) {
        return Err(ProviderError::InvalidRequest);
    }
    if !is_valid_url_path(path) {
        return Err(ProviderError::InvalidRequest);
    }
    Ok(ParsedUrl {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
        path: path.to_owned(),
    })
}

fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    host.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

fn is_valid_url_path(path: &str) -> bool {
    path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_graphic() || b == b'/' || b == b'-' || b == b'_' || b == b'.')
}

fn host_is_blocked(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if matches!(
        host,
        "169.254.169.254" | "metadata" | "metadata.google.internal" | "metadata.goog"
    ) || host.starts_with("169.254.")
    {
        return true;
    }
    // inet_aton encodings (dword/hex/octal) of link-local/metadata (T-005).
    if let Some(ip) = decode_ipv4_literal(host) {
        return ip_is_blocked(IpAddr::V4(ip));
    }
    false
}

fn decode_ipv4_literal(host: &str) -> Option<Ipv4Addr> {
    if host.is_empty() || host.len() > 63 {
        return None;
    }
    let mut parts = [0u64; 4];
    let mut count = 0usize;
    for part in host.split('.') {
        if count >= 4 {
            return None;
        }
        parts[count] = parse_ipv4_component(part)?;
        count += 1;
    }
    let octets = match count {
        1 => {
            let dword = parts[0];
            if dword > u64::from(u32::MAX) {
                return None;
            }
            let value = dword as u32;
            [
                (value >> 24) as u8,
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ]
        }
        2 => {
            let a = parts[0];
            let rest = parts[1];
            if a > 0xff || rest > 0x00ff_ffff {
                return None;
            }
            [a as u8, (rest >> 16) as u8, (rest >> 8) as u8, rest as u8]
        }
        3 => {
            let a = parts[0];
            let b = parts[1];
            let rest = parts[2];
            if a > 0xff || b > 0xff || rest > 0xffff {
                return None;
            }
            [a as u8, b as u8, (rest >> 8) as u8, rest as u8]
        }
        4 => {
            let [a, b, c, d] = parts;
            if a > 0xff || b > 0xff || c > 0xff || d > 0xff {
                return None;
            }
            [a as u8, b as u8, c as u8, d as u8]
        }
        _ => return None,
    };
    Some(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]))
}

fn parse_ipv4_component(raw: &str) -> Option<u64> {
    if raw.is_empty() || raw.len() > 16 {
        return None;
    }
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return u64::from_str_radix(hex, 16).ok();
    }
    if raw.len() > 1 && raw.starts_with('0') {
        if !raw.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return None;
        }
        return u64::from_str_radix(raw, 8).ok();
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (octets[0] == 169 && octets[1] == 254)
        }
        IpAddr::V6(v6) => {
            v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
        }
    }
}

fn slice_timeout(total: Duration) -> Duration {
    let slice = Duration::from_millis(100);
    if total < slice { total } else { slice }
}

fn write_http_request(
    stream: &mut TcpStream,
    url: &ParsedUrl,
    headers: &[(String, String)],
    body: &[u8],
    bearer: &str,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), ProviderError> {
    cancel.check()?;
    check_deadline(deadline)?;
    let host = if (url.scheme == UrlScheme::Http && url.port == 80)
        || (url.scheme == UrlScheme::Https && url.port == 443)
    {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        url.path,
        host,
        bearer,
        body.len()
    );
    for (name, value) in headers {
        if !is_safe_header(name, value) {
            return Err(ProviderError::InvalidRequest);
        }
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    write_all_deadline(stream, request.as_bytes(), cancel, deadline)?;
    write_all_deadline(stream, body, cancel, deadline)
}

fn is_safe_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.eq_ignore_ascii_case("authorization")
        && value.bytes().all(|b| b >= 0x20 && b != 0x7f)
        && name.len() <= 64
        && value.len() <= MAX_HEADER_LINE_BYTES
}

fn read_http_response(
    stream: &mut TcpStream,
    max_body: usize,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<ProviderHttpResponse, ProviderError> {
    let raw = read_until_limit(stream, max_body + 16 * 1024, cancel, deadline)?;
    let split = find_header_body_split(&raw).ok_or(ProviderError::Permanent)?;
    let header_bytes = &raw[..split];
    let body_prefix = &raw[split + 4..];
    let header_text = std::str::from_utf8(header_bytes).map_err(|_| ProviderError::Permanent)?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().ok_or(ProviderError::Permanent)?;
    let status = parse_status_line(status_line)?;
    let mut headers = Vec::new();
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if headers.len() >= MAX_RESPONSE_HEADERS || line.len() > MAX_HEADER_LINE_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        let (name, value) = line.split_once(':').ok_or(ProviderError::Permanent)?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| ProviderError::Permanent)?,
            );
        }
        if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            return Err(ProviderError::Permanent);
        }
        headers.push((name, value));
    }

    let mut body = body_prefix.to_vec();
    if let Some(length) = content_length {
        if length > max_body {
            return Err(ProviderError::BoundExceeded);
        }
        while body.len() < length {
            let mut buf = [0u8; 2048];
            let want = (length - body.len()).min(buf.len());
            let read = read_some(stream, &mut buf[..want], cancel, deadline)?;
            if read == 0 {
                // Peer closed short of Content-Length: fail closed.
                return Err(ProviderError::Permanent);
            }
            body.extend_from_slice(&buf[..read]);
        }
        if body.len() > length {
            body.truncate(length);
        }
    } else {
        // No Content-Length: read until EOF, deadline, or max. A prefix is not success.
        loop {
            if body.len() > max_body {
                return Err(ProviderError::BoundExceeded);
            }
            let mut buf = [0u8; 2048];
            let room = max_body.saturating_sub(body.len());
            if room == 0 {
                let mut probe = [0u8; 1];
                let extra = read_some(stream, &mut probe, cancel, deadline)?;
                if extra > 0 {
                    return Err(ProviderError::BoundExceeded);
                }
                break;
            }
            let want = room.min(buf.len());
            let read = read_some(stream, &mut buf[..want], cancel, deadline)?;
            if read == 0 {
                break;
            }
            if body.len() + read > max_body {
                return Err(ProviderError::BoundExceeded);
            }
            body.extend_from_slice(&buf[..read]);
        }
    }
    if body.len() > max_body {
        return Err(ProviderError::BoundExceeded);
    }
    ProviderHttpResponse::new(status, headers, body)
}

fn parse_status_line(line: &str) -> Result<u16, ProviderError> {
    let mut parts = line.split(' ');
    let version = parts.next().ok_or(ProviderError::Permanent)?;
    if !version.starts_with("HTTP/1.") {
        return Err(ProviderError::Permanent);
    }
    let status = parts.next().ok_or(ProviderError::Permanent)?;
    status.parse::<u16>().map_err(|_| ProviderError::Permanent)
}

fn find_header_body_split(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_until_limit(
    stream: &mut TcpStream,
    limit: usize,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = read_some(stream, &mut chunk, cancel, deadline)?;
        if n == 0 {
            break;
        }
        if buf.len() + n > limit {
            return Err(ProviderError::BoundExceeded);
        }
        buf.extend_from_slice(&chunk[..n]);
        if find_header_body_split(&buf).is_some() {
            break;
        }
    }
    Ok(buf)
}

fn read_some(
    stream: &mut TcpStream,
    buf: &mut [u8],
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<usize, ProviderError> {
    loop {
        cancel.check()?;
        check_deadline(deadline)?;
        match stream.read(buf) {
            Ok(n) => return Ok(n),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(err) => return Err(map_io_error(err)),
        }
    }
}

fn write_all_deadline(
    stream: &mut TcpStream,
    mut data: &[u8],
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), ProviderError> {
    while !data.is_empty() {
        cancel.check()?;
        check_deadline(deadline)?;
        match stream.write(data) {
            Ok(0) => return Err(ProviderError::Transient),
            Ok(n) => data = &data[n..],
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(err) => return Err(map_io_error(err)),
        }
    }
    Ok(())
}

fn check_deadline(deadline: Instant) -> Result<(), ProviderError> {
    if Instant::now() >= deadline {
        Err(ProviderError::Transient)
    } else {
        Ok(())
    }
}

fn map_io_error(err: std::io::Error) -> ProviderError {
    match err.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ProviderError::Transient,
        std::io::ErrorKind::Interrupted => ProviderError::Transient,
        _ => ProviderError::Transient,
    }
}

impl Debug for OpenAiCompatibleConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatibleConfig")
            .field("profile", &self.profile)
            .field("endpoint", &self.endpoint)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl Debug for OpenAiCompatibleEndpoint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatibleEndpoint")
            .field("base_url", &self.base_url)
            .field("style", &self.style)
            .finish()
    }
}

impl<T> Debug for OpenAiCompatibleAdapter<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatibleAdapter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Debug for StaticWireAuth {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticWireAuth")
            .field("redacted", &true)
            .field("byte_len", &self.token.len())
            .finish()
    }
}

impl Debug for ProviderHttpResponse {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderHttpResponse")
            .field("status", &self.status)
            .field("header_count", &self.headers.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl fmt::Display for OpenAiApiStyle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    use auth::{
        CredentialKind, CredentialPut, CredentialStore, InMemoryCredentialStore, SecretRef,
        SecretValue,
    };
    use protocol::{
        ArtifactId, ArtifactRef, Baggage, ErrorCode, RedactionClass, TraceContext, TraceId,
    };

    use crate::credentials::CredentialResolver;
    use crate::provider::{
        CanonicalMessage, CatalogRevision, ContentPart, ModelId, ModelPurpose, ModelRef,
        ModelRequestId, ProviderId, ReasoningSupport, ToolCall, ToolCallId, ToolName,
        UsageFieldSet,
    };

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-9f3c2a";
    const FIXTURE_TOKEN: &str = "fixture-test-token";
    const REF_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const TRACE: &str = "0193e0f6-6c3a-7d1e-8b2c-4d5e6f708192";

    struct FixtureServer {
        addr: std::net::SocketAddr,
        captured: Arc<Mutex<Option<CapturedRequest>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    struct CapturedRequest {
        method: String,
        path: String,
        body: String,
        has_authorization: bool,
        authorization_is_fixture: bool,
        authorization_has_canary: bool,
    }

    struct FixtureScript {
        status: u16,
        body: String,
        extra_headers: Vec<(String, String)>,
    }

    impl FixtureServer {
        fn spawn(script: FixtureScript) -> Self {
            Self::spawn_inner(script, true)
        }

        fn spawn_no_content_length(script: FixtureScript) -> Self {
            Self::spawn_inner(script, false)
        }

        fn spawn_hanging() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging");
            listener.set_nonblocking(true).expect("nonblocking");
            let addr = listener.local_addr().expect("addr");
            let captured = Arc::new(Mutex::new(None));
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_thread = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut held = None;
                while !shutdown_thread.load(Ordering::SeqCst) && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            held = Some(stream);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                    if held.is_some() {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                drop(held);
            });
            Self {
                addr,
                captured,
                shutdown,
                handle: Some(handle),
            }
        }

        fn spawn_inner(script: FixtureScript, include_content_length: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
            listener.set_nonblocking(true).expect("nonblocking");
            let addr = listener.local_addr().expect("addr");
            let captured = Arc::new(Mutex::new(None));
            let shutdown = Arc::new(AtomicBool::new(false));
            let captured_thread = Arc::clone(&captured);
            let shutdown_thread = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if shutdown_thread.load(Ordering::SeqCst) || Instant::now() >= deadline {
                        break;
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                            let mut buf = vec![0u8; 64 * 1024];
                            let n = stream.read(&mut buf).unwrap_or(0);
                            let raw = String::from_utf8_lossy(&buf[..n]);
                            if let Some(captured_req) = parse_captured(&raw) {
                                *captured_thread.lock().expect("capture lock") = Some(captured_req);
                            }
                            let extra = script
                                .extra_headers
                                .iter()
                                .map(|(k, v)| format!("{k}: {v}\r\n"))
                                .collect::<String>();
                            let length_header = if include_content_length {
                                format!("Content-Length: {}\r\n", script.body.len())
                            } else {
                                String::new()
                            };
                            let response = format!(
                                "HTTP/1.1 {} TEST\r\nContent-Type: text/event-stream\r\n{length_header}Connection: close\r\n{extra}\r\n{}",
                                script.status, script.body
                            );
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                addr,
                captured,
                shutdown,
                handle: Some(handle),
            }
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}/v1", self.addr.port())
        }

        fn captured(&self) -> CapturedRequest {
            self.captured
                .lock()
                .expect("lock")
                .clone()
                .expect("captured request")
        }
    }

    impl Drop for FixtureServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl Clone for CapturedRequest {
        fn clone(&self) -> Self {
            Self {
                method: self.method.clone(),
                path: self.path.clone(),
                body: self.body.clone(),
                has_authorization: self.has_authorization,
                authorization_is_fixture: self.authorization_is_fixture,
                authorization_has_canary: self.authorization_has_canary,
            }
        }
    }

    fn parse_captured(raw: &str) -> Option<CapturedRequest> {
        let (headers, body) = raw.split_once("\r\n\r\n")?;
        let mut lines = headers.lines();
        let request_line = lines.next()?;
        let mut parts = request_line.split(' ');
        let method = parts.next()?.to_owned();
        let path = parts.next()?.to_owned();
        let mut has_authorization = false;
        let mut authorization_is_fixture = false;
        let mut authorization_has_canary = false;
        for line in lines {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("authorization")
            {
                has_authorization = true;
                let value = value.trim();
                authorization_is_fixture = value == format!("Bearer {FIXTURE_TOKEN}");
                authorization_has_canary = value.contains(CANARY);
            }
        }
        Some(CapturedRequest {
            method,
            path,
            body: body.to_owned(),
            has_authorization,
            authorization_is_fixture,
            authorization_has_canary,
        })
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn openai() -> ProviderId {
        ProviderId::parse("openai").expect("provider")
    }

    fn sample_ref() -> SecretRef {
        SecretRef::from_id_and_alias(REF_ID, "env:OPENAI_API_KEY").expect("ref")
    }

    fn caps(tools: bool, vision: bool) -> ProviderCapabilities {
        ProviderCapabilities::new(
            tools,
            true,
            vision,
            true,
            ReasoningSupport::Exposed,
            false,
            128_000,
            8192,
            UsageFieldSet::new(true, true, true, true, true, false, false),
        )
        .expect("caps")
    }

    fn store_with_canary() -> InMemoryCredentialStore {
        let store = InMemoryCredentialStore::new();
        let value = SecretValue::from_str(sample_ref(), CANARY).expect("secret");
        let put = CredentialPut::ephemeral(value, CredentialKind::ProviderApiKey)
            .with_provider("openai")
            .expect("provider");
        store
            .put(put, &auth::CancellationToken::new())
            .expect("put");
        store
    }

    fn profile() -> ProviderProfile {
        ProviderProfile::new("default", openai(), sample_ref()).expect("profile")
    }

    fn config(
        base: &str,
        style: OpenAiApiStyle,
        capabilities: ProviderCapabilities,
    ) -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig::new(
            profile(),
            OpenAiCompatibleEndpoint::new(base, style).expect("endpoint"),
            capabilities,
        )
        .expect("config")
    }

    fn adapter<'a>(
        store: &'a InMemoryCredentialStore,
        base: &str,
        style: OpenAiApiStyle,
        capabilities: ProviderCapabilities,
    ) -> OpenAiCompatibleAdapter<'a, Http1Transport<StaticWireAuth>> {
        let transport = Http1Transport::with_limits(
            StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"),
            Duration::from_secs(3),
            MAX_HTTP_RESPONSE_BYTES,
        );
        OpenAiCompatibleAdapter::new(config(base, style, capabilities), transport, store)
    }

    fn request(tools: bool, vision: bool) -> CanonicalModelRequest {
        let mut messages = vec![
            CanonicalMessage::new(
                MessageRole::System,
                vec![ContentPart::text("sys").expect("sys")],
                None,
                vec![],
            )
            .expect("sys"),
        ];
        if vision {
            messages.push(
                CanonicalMessage::new(
                    MessageRole::User,
                    vec![
                        ContentPart::text("see").expect("text"),
                        ContentPart::image(ArtifactRef::new(
                            ArtifactId::from_bytes(b"vision-fixture"),
                            "image/png",
                            16,
                            RedactionClass::Project,
                        )),
                    ],
                    None,
                    vec![],
                )
                .expect("user"),
            );
        } else {
            messages.push(
                CanonicalMessage::new(
                    MessageRole::User,
                    vec![ContentPart::text("hello").expect("text")],
                    None,
                    vec![],
                )
                .expect("user"),
            );
        }
        let tools = if tools {
            vec![
                CanonicalToolSpec::new(
                    ToolName::parse("search").expect("t1"),
                    "find",
                    serde_json::json!({"type":"object","properties":{}}),
                )
                .expect("search"),
                CanonicalToolSpec::new(
                    ToolName::parse("read").expect("t2"),
                    "open",
                    serde_json::json!({"type":"object","properties":{}}),
                )
                .expect("read"),
            ]
        } else {
            vec![]
        };
        CanonicalModelRequest::new(
            ModelRequestId::parse("req-openai-1").expect("id"),
            ModelRef::new(openai(), ModelId::parse("gpt-4.1").expect("model")),
            ModelPurpose::Code,
            messages,
            tools,
            Some(256),
            CatalogRevision::new(7).expect("rev"),
            TraceContext::new(
                TRACE.parse::<TraceId>().expect("trace"),
                None,
                Baggage::empty(),
            ),
            &live(),
        )
        .expect("request")
    }

    fn sse_text() -> String {
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\r\n\r\n\
         data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\r\n\r\n\
         data: [DONE]\r\n\r\n"
            .to_owned()
    }

    fn sse_tools() -> String {
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\"\"}}]}}]}\r\n\r\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2}}\r\n\r\n\
         data: [DONE]\r\n\r\n"
            .to_owned()
    }

    fn sse_responses() -> String {
        "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n"
            .to_owned()
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut fut = std::pin::pin!(fut);
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("adapter future must complete without an executor"),
        }
    }

    fn assert_no_canary(label: &str, rendered: &str) {
        assert!(
            !rendered.contains(CANARY),
            "{label} leaked secret plaintext: {rendered}"
        );
        assert!(
            !rendered.contains("PLAINTEXT-do-not-leak"),
            "{label} leaked secret substring: {rendered}"
        );
    }

    #[test]
    fn chat_stream_emits_normalized_text_and_usage() {
        let server = FixtureServer::spawn(FixtureScript {
            status: 200,
            body: sse_text(),
            extra_headers: vec![],
        });
        let store = store_with_canary();
        let adapter = adapter(
            &store,
            &server.base_url(),
            OpenAiApiStyle::ChatCompletions,
            caps(false, false),
        );
        let stream = block_on(adapter.invoke(request(false, false), live())).expect("invoke");
        let events = stream.events();
        assert!(
            events.iter().any(
                |event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "Hello")
            )
        );
        assert!(events.iter().any(
            |event| matches!(event, ModelStreamEvent::TextDelta { text } if text == " world")
        ));
        let usage = stream.terminal_usage().expect("usage");
        assert_eq!(usage.input_tokens(), Some(12));
        assert_eq!(usage.cached_input_tokens(), Some(4));
        assert_eq!(usage.uncached_input_tokens(), Some(8));
        assert_eq!(usage.output_tokens(), Some(3));
        assert_eq!(usage.cost(), UsageCost::Unknown);
        assert_ne!(usage.cost(), UsageCost::Reported { usd_micros: 0 });
        let captured = server.captured();
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.path, "/v1/chat/completions");
        assert!(captured.has_authorization);
        assert!(captured.authorization_is_fixture);
        assert!(!captured.authorization_has_canary);
        assert!(!captured.body.contains(CANARY));
        let payload: Value = serde_json::from_str(&captured.body).expect("json");
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["model"], "gpt-4.1");
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][1]["role"], "user");
    }

    #[test]
    fn chat_stream_preserves_tool_order_and_call_deltas() {
        let server = FixtureServer::spawn(FixtureScript {
            status: 200,
            body: sse_tools(),
            extra_headers: vec![],
        });
        let store = store_with_canary();
        let adapter = adapter(
            &store,
            &server.base_url(),
            OpenAiApiStyle::ChatCompletions,
            caps(true, false),
        );
        let req = request(true, false);
        let encoded = encode_provider_payload(&req, OpenAiApiStyle::ChatCompletions, &live())
            .expect("encode");
        assert_eq!(encoded["tools"][0]["function"]["name"], "search");
        assert_eq!(encoded["tools"][1]["function"]["name"], "read");
        let stream = block_on(adapter.invoke(req, live())).expect("invoke");
        let mut saw_start = false;
        let mut args = String::new();
        let mut finish = None;
        for event in stream.events() {
            match event {
                ModelStreamEvent::ToolCallStart { call_id, name } => {
                    assert_eq!(call_id.as_str(), "call_1");
                    assert_eq!(name.as_str(), "search");
                    saw_start = true;
                }
                ModelStreamEvent::ToolCallArgumentsDelta {
                    call_id,
                    arguments_delta,
                } => {
                    assert_eq!(call_id.as_str(), "call_1");
                    args.push_str(arguments_delta);
                }
                ModelStreamEvent::Completed { finish: reason, .. } => finish = Some(*reason),
                _ => {}
            }
        }
        assert!(saw_start);
        assert_eq!(args, "{\"q\":1}");
        assert_eq!(finish, Some(FinishReason::ToolCalls));
    }

    #[test]
    fn responses_style_uses_input_and_normalizes_text() {
        let server = FixtureServer::spawn(FixtureScript {
            status: 200,
            body: sse_responses(),
            extra_headers: vec![],
        });
        let store = store_with_canary();
        let adapter = adapter(
            &store,
            &server.base_url(),
            OpenAiApiStyle::Responses,
            caps(true, false),
        );
        let req = request(true, false);
        let encoded =
            encode_provider_payload(&req, OpenAiApiStyle::Responses, &live()).expect("encode");
        assert!(encoded.get("messages").is_none());
        assert_eq!(encoded["input"][0]["role"], "system");
        assert_eq!(encoded["tools"][0]["name"], "search");
        assert_eq!(encoded["tools"][1]["name"], "read");
        let stream = block_on(adapter.invoke(req, live())).expect("invoke");
        assert!(
            stream
                .events()
                .iter()
                .any(|event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "Hi"))
        );
        assert_eq!(server.captured().path, "/v1/responses");
        let usage = stream.terminal_usage().expect("usage");
        assert_eq!(usage.input_tokens(), Some(5));
        assert_eq!(usage.output_tokens(), Some(1));
        assert_eq!(usage.cost(), UsageCost::Unknown);
    }

    #[test]
    fn rate_limit_and_auth_and_context_map_to_typed_errors() {
        let store = store_with_canary();

        let limited = FixtureServer::spawn(FixtureScript {
            status: 429,
            body: format!(r#"{{"error":{{"message":"rate {CANARY}","type":"rate_limit_error"}}}}"#),
            extra_headers: vec![("Retry-After".to_owned(), "2".to_owned())],
        });
        let err = block_on(
            adapter(
                &store,
                &limited.base_url(),
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            )
            .invoke(request(false, false), live()),
        )
        .expect_err("rate");
        assert_eq!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: Some(2000)
            }
        );
        assert_eq!(err.code(), Some(ErrorCode::ProviderRateLimited));
        assert!(err.is_retryable());
        assert_no_canary("rate", &format!("{err:?}{err}"));

        let auth = FixtureServer::spawn(FixtureScript {
            status: 401,
            body: format!(
                r#"{{"error":{{"message":"Incorrect API key provided: {CANARY}","type":"invalid_request_error","code":"invalid_api_key"}}}}"#
            ),
            extra_headers: vec![],
        });
        let err = block_on(
            adapter(
                &store,
                &auth.base_url(),
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            )
            .invoke(request(false, false), live()),
        )
        .expect_err("auth");
        assert_eq!(err, ProviderError::AuthFailed);
        assert_eq!(err.code(), Some(ErrorCode::ProviderAuthFailed));
        assert!(!err.is_retryable());
        assert_no_canary("auth", &format!("{err:?}{err}"));

        let too_big = FixtureServer::spawn(FixtureScript {
            status: 400,
            body: format!(
                r#"{{"error":{{"message":"max context {CANARY}","type":"invalid_request_error","code":"context_length_exceeded"}}}}"#
            ),
            extra_headers: vec![],
        });
        let err = block_on(
            adapter(
                &store,
                &too_big.base_url(),
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            )
            .invoke(request(false, false), live()),
        )
        .expect_err("context");
        assert_eq!(err, ProviderError::ContextTooLarge);
        assert_eq!(err.code(), Some(ErrorCode::ProviderContextTooLarge));
        assert!(!err.is_retryable());
        assert_no_canary("context", &format!("{err:?}{err}"));
    }

    #[test]
    fn auth_failure_is_not_remapped_to_transient() {
        let store = store_with_canary();
        let server = FixtureServer::spawn(FixtureScript {
            status: 403,
            body: format!(r#"{{"error":{{"message":"{CANARY}","type":"access_denied"}}}}"#),
            extra_headers: vec![],
        });
        let err = block_on(
            adapter(
                &store,
                &server.base_url(),
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            )
            .invoke(request(false, false), live()),
        )
        .expect_err("forbidden");
        assert_eq!(err, ProviderError::AuthFailed);
        assert_ne!(err, ProviderError::Transient);
        assert!(!err.is_retryable());
        assert_no_canary("forbidden", &format!("{err}"));
    }

    #[test]
    fn missing_credential_is_auth_failed_without_network() {
        let store = InMemoryCredentialStore::new();
        let transport = Http1Transport::new(StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"));
        let adapter = OpenAiCompatibleAdapter::new(
            config(
                "http://127.0.0.1:1/v1",
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            ),
            transport,
            &store,
        );
        let err = adapter
            .invoke_sync(request(false, false), &live())
            .expect_err("missing");
        assert_eq!(err, ProviderError::AuthFailed);
        assert_no_canary("missing", &format!("{err:?}"));
    }

    #[test]
    fn cancellation_is_not_swallowed() {
        let store = store_with_canary();
        let transport = Http1Transport::new(StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"));
        let adapter = OpenAiCompatibleAdapter::new(
            config(
                "http://127.0.0.1:1/v1",
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            ),
            transport,
            &store,
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = adapter
            .invoke_sync(request(false, false), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, ProviderError::Cancelled);
        assert!(err.code().is_none());
    }

    #[test]
    fn metadata_and_userinfo_urls_are_rejected() {
        assert_eq!(
            OpenAiCompatibleEndpoint::new(
                "http://169.254.169.254/v1",
                OpenAiApiStyle::ChatCompletions
            )
            .expect_err("metadata"),
            ProviderError::InvalidRequest
        );
        assert_eq!(
            OpenAiCompatibleEndpoint::new(
                format!("http://user:{CANARY}@127.0.0.1/v1"),
                OpenAiApiStyle::ChatCompletions
            )
            .expect_err("userinfo"),
            ProviderError::InvalidRequest
        );
        assert_eq!(
            OpenAiCompatibleEndpoint::new(
                "https://api.openai.com/v1",
                OpenAiApiStyle::ChatCompletions
            )
            .expect("https config")
            .style(),
            OpenAiApiStyle::ChatCompletions
        );
        let store = store_with_canary();
        let transport = Http1Transport::new(StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"));
        let adapter = OpenAiCompatibleAdapter::new(
            OpenAiCompatibleConfig::new(
                profile(),
                OpenAiCompatibleEndpoint::new(
                    "https://api.openai.com/v1",
                    OpenAiApiStyle::ChatCompletions,
                )
                .expect("https"),
                caps(false, false),
            )
            .expect("cfg"),
            transport,
            &store,
        );
        let err = adapter
            .invoke_sync(request(false, false), &live())
            .expect_err("no tls");
        assert_eq!(err, ProviderError::InvalidRequest);
    }

    #[test]
    fn image_parts_are_artifact_refs_not_bytes() {
        let req = request(false, true);
        let encoded = encode_provider_payload(&req, OpenAiApiStyle::ChatCompletions, &live())
            .expect("encode");
        let content = &encoded["messages"][1]["content"];
        assert_eq!(content[1]["type"], "image_url");
        let url = content[1]["image_url"]["url"].as_str().expect("url");
        assert!(url.starts_with("artifact:sha256:"));
        assert!(!url.contains("PNG"));
        assert_eq!(content[1].get("bytes"), None);
    }

    #[test]
    fn oversized_response_is_bound_exceeded() {
        let huge = format!("data: {}\n\n", "x".repeat(MAX_HTTP_RESPONSE_BYTES + 8));
        let server = FixtureServer::spawn(FixtureScript {
            status: 200,
            body: huge,
            extra_headers: vec![],
        });
        let store = store_with_canary();
        let transport = Http1Transport::with_limits(
            StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"),
            Duration::from_secs(3),
            1024,
        );
        let adapter = OpenAiCompatibleAdapter::new(
            config(
                &server.base_url(),
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            ),
            transport,
            &store,
        );
        let err = adapter
            .invoke_sync(request(false, false), &live())
            .expect_err("bound");
        assert_eq!(err, ProviderError::BoundExceeded);
    }

    #[test]
    fn adapter_debug_redacts_credential_plaintext() {
        let store = store_with_canary();
        let transport = Http1Transport::new(StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"));
        let adapter = OpenAiCompatibleAdapter::new(
            config(
                "http://127.0.0.1:9/v1",
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            ),
            transport,
            &store,
        );
        assert_no_canary("adapter debug", &format!("{adapter:?}"));
        assert_no_canary("config debug", &format!("{:?}", adapter.config()));
        assert_no_canary(
            "static auth",
            &format!("{:?}", StaticWireAuth::bearer(CANARY).expect("token")),
        );
    }

    #[test]
    fn assistant_tool_result_order_is_preserved() {
        let messages = vec![
            CanonicalMessage::new(
                MessageRole::User,
                vec![ContentPart::text("go").expect("t")],
                None,
                vec![],
            )
            .expect("user"),
            CanonicalMessage::new(
                MessageRole::Assistant,
                vec![],
                None,
                vec![
                    ToolCall::new(
                        ToolCallId::parse("call_a").expect("a"),
                        ToolName::parse("search").expect("s"),
                        "{\"q\":1}",
                    )
                    .expect("call a"),
                    ToolCall::new(
                        ToolCallId::parse("call_b").expect("b"),
                        ToolName::parse("read").expect("r"),
                        "{\"p\":2}",
                    )
                    .expect("call b"),
                ],
            )
            .expect("assistant"),
            CanonicalMessage::new(
                MessageRole::Tool,
                vec![ContentPart::text("A").expect("a")],
                Some(ToolCallId::parse("call_a").expect("a")),
                vec![],
            )
            .expect("tool a"),
            CanonicalMessage::new(
                MessageRole::Tool,
                vec![ContentPart::text("B").expect("b")],
                Some(ToolCallId::parse("call_b").expect("b")),
                vec![],
            )
            .expect("tool b"),
        ];
        let req = CanonicalModelRequest::new(
            ModelRequestId::parse("req-order").expect("id"),
            ModelRef::new(openai(), ModelId::parse("gpt-4.1").expect("model")),
            ModelPurpose::Chat,
            messages,
            vec![],
            None,
            CatalogRevision::new(1).expect("rev"),
            TraceContext::new(
                TRACE.parse::<TraceId>().expect("trace"),
                None,
                Baggage::empty(),
            ),
            &live(),
        )
        .expect("req");
        let chat =
            encode_provider_payload(&req, OpenAiApiStyle::ChatCompletions, &live()).expect("chat");
        assert_eq!(chat["messages"][1]["tool_calls"][0]["id"], "call_a");
        assert_eq!(chat["messages"][1]["tool_calls"][1]["id"], "call_b");
        assert_eq!(chat["messages"][2]["tool_call_id"], "call_a");
        assert_eq!(chat["messages"][3]["tool_call_id"], "call_b");
        let responses =
            encode_provider_payload(&req, OpenAiApiStyle::Responses, &live()).expect("resp");
        assert_eq!(responses["input"][1]["type"], "function_call");
        assert_eq!(responses["input"][1]["call_id"], "call_a");
    }

    #[test]
    fn split_no_content_length_sse_is_rejected() {
        let prefix = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\r\n\r\n";
        let server = FixtureServer::spawn_no_content_length(FixtureScript {
            status: 200,
            body: prefix.to_owned(),
            extra_headers: vec![],
        });
        let store = store_with_canary();
        let adapter = adapter(
            &store,
            &server.base_url(),
            OpenAiApiStyle::ChatCompletions,
            caps(false, false),
        );
        let err = block_on(adapter.invoke(request(false, false), live())).expect_err("prefix");
        assert!(
            matches!(
                err,
                ProviderError::Permanent | ProviderError::Transient | ProviderError::BoundExceeded
            ),
            "truncated no-CL SSE must not complete: {err:?}"
        );
        assert_ne!(err, ProviderError::AuthFailed);
    }

    #[test]
    fn hanging_peer_times_out_as_transient() {
        let server = FixtureServer::spawn_hanging();
        let store = store_with_canary();
        let transport = Http1Transport::with_limits(
            StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"),
            Duration::from_millis(400),
            MAX_HTTP_RESPONSE_BYTES,
        );
        let adapter = OpenAiCompatibleAdapter::new(
            config(
                &server.base_url(),
                OpenAiApiStyle::ChatCompletions,
                caps(false, false),
            ),
            transport,
            &store,
        );
        let started = Instant::now();
        let err = adapter
            .invoke_sync(request(false, false), &live())
            .expect_err("hang");
        assert!(
            matches!(err, ProviderError::Transient | ProviderError::Cancelled),
            "hanging peer must not succeed: {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "hanging peer must not block past the transport deadline"
        );
    }

    #[test]
    fn encoded_imds_hosts_are_invalid_request_at_execute() {
        let store = store_with_canary();
        let resolver = CredentialResolver::new(&store);
        let credential = resolver
            .resolve(&openai(), &profile(), &live())
            .expect("credential");
        let transport = Http1Transport::with_limits(
            StaticWireAuth::bearer(FIXTURE_TOKEN).expect("auth"),
            Duration::from_millis(400),
            MAX_HTTP_RESPONSE_BYTES,
        );
        let headers = [("content-type".to_owned(), "application/json".to_owned())];
        let body = b"{}";
        // 169.254.169.254 as dword / hex / octal (T-005).
        let hosts = [
            "2852039166",
            "0xa9fea9fe",
            "0251.0376.0251.0376",
            "0xA9.0xFE.0xA9.0xFE",
        ];
        for host in hosts {
            let url = format!("http://{host}/latest/meta-data");
            let request = ProviderHttpRequest {
                url: &url,
                headers: &headers,
                body,
                credential: &credential,
            };
            let err = transport.execute(&request, &live()).expect_err(host);
            assert_eq!(
                err,
                ProviderError::InvalidRequest,
                "encoded IMDS host {host} must fail closed at execute"
            );
        }
    }
}
