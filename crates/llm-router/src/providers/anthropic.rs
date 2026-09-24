//! Anthropic Messages API streaming adapter.
//!
//! Transport conversion only: compiled messages/tools become the Messages
//! API envelope (`system`, `tool_use`/`tool_result`, `input_schema`).
//! API-key plaintext is attached by the HTTP transport and is never stored
//! on model/config objects. Provider error bodies are discarded after
//! classification (threats `T-012`, `T-001`). Endpoint construction reuses
//! the OpenAI-compatible origin checks so IMDS/userinfo stay rejected (`T-005`).

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;

use serde_json::{Map, Value};

use crate::credentials::{CredentialResolver, ProviderProfile};
use crate::provider::{
    CancellationToken, CanonicalMessage, CanonicalModelRequest, CanonicalToolSpec, ContentPart,
    FinishReason, MAX_STREAM_DELTA_BYTES, MAX_STREAM_EVENTS, MessageRole, ModelStream,
    ModelStreamEvent, NormalizedUsage, ProviderAdapter, ProviderCapabilities, ProviderError,
    ToolCallId, ToolName, UsageCost, UsageExtValue,
};
use crate::providers::openai_compatible::{
    HttpTransport, MAX_HTTP_REQUEST_BYTES, OpenAiApiStyle, OpenAiCompatibleEndpoint,
    ProviderHttpRequest,
};

/// Wire schema name for [`AnthropicConfig`].
pub const ANTHROPIC_CONFIG_SCHEMA: &str = "rapidlm.anthropic_config";

/// v1 schema version for Anthropic adapter config.
pub const ANTHROPIC_SCHEMA_VERSION: u16 = 1;

/// Messages API version sent on every request.
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Path appended to the validated base URL.
pub const ANTHROPIC_MESSAGES_PATH: &str = "/messages";

/// How often to honor cancellation while encoding or parsing.
const CANCEL_CHECK_EVERY: usize = 16;

/// Handle-only profile plus the Messages API origin it addresses.
#[derive(Clone, Eq, PartialEq)]
pub struct AnthropicConfig {
    profile: ProviderProfile,
    endpoint: AnthropicEndpoint,
    capabilities: ProviderCapabilities,
}

/// Validated origin + prefix for an Anthropic-compatible Messages server.
#[derive(Clone, Eq, PartialEq)]
pub struct AnthropicEndpoint {
    base_url: String,
}

/// Streaming adapter for one Anthropic provider profile.
pub struct AnthropicAdapter<'store, T> {
    config: AnthropicConfig,
    transport: T,
    store: &'store dyn auth::CredentialStore,
}

impl AnthropicEndpoint {
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, ProviderError> {
        // Reuse the shared origin validator (IMDS, userinfo, encoded IPv4).
        let validated = OpenAiCompatibleEndpoint::new(base_url, OpenAiApiStyle::ChatCompletions)?;
        Ok(Self {
            base_url: validated.base_url().to_owned(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn request_url(&self) -> String {
        format!(
            "{}{ANTHROPIC_MESSAGES_PATH}",
            self.base_url.trim_end_matches('/')
        )
    }
}

impl AnthropicConfig {
    pub fn new(
        profile: ProviderProfile,
        endpoint: AnthropicEndpoint,
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

    pub fn endpoint(&self) -> &AnthropicEndpoint {
        &self.endpoint
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
}

impl<'store, T: HttpTransport> AnthropicAdapter<'store, T> {
    pub fn new(
        config: AnthropicConfig,
        transport: T,
        store: &'store dyn auth::CredentialStore,
    ) -> Self {
        Self {
            config,
            transport,
            store,
        }
    }

    pub fn config(&self) -> &AnthropicConfig {
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
        let max_tokens = req
            .max_output_tokens()
            .unwrap_or_else(|| self.config.capabilities.max_output());
        let body = encode_anthropic_payload(&req, max_tokens, cancel)?;
        let encoded = serde_json::to_vec(&body).map_err(|_| ProviderError::InvalidRequest)?;
        if encoded.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(ProviderError::BoundExceeded);
        }

        let url = self.config.endpoint.request_url();
        let request_id = req.request_id().as_str().to_owned();
        let headers = [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            (
                "anthropic-version".to_owned(),
                ANTHROPIC_API_VERSION.to_owned(),
            ),
            ("x-rapidlm-request-id".to_owned(), request_id),
        ];
        cancel.check()?;
        let outbound = ProviderHttpRequest::new(&url, &headers, &encoded, &credential);
        let response = self.transport.execute(&outbound, cancel)?;
        drop(credential);
        classify_http_error(&response)?;
        let events = parse_anthropic_stream(response.body(), cancel)?;
        ModelStream::from_events(
            req.request_id().clone(),
            req.model().clone(),
            events,
            cancel,
        )
    }

    /// Progressive delivery: the transport's streaming path feeds chunks to
    /// an SSE delta parser as they arrive (`content_block_delta` text
    /// forwarded to `on_text`), then the WHOLE body is canonically parsed
    /// exactly as the buffered path — the wire contract is unchanged, only
    /// when the text becomes visible differs.
    pub fn invoke_sync_streaming(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<ModelStream, ProviderError> {
        cancel.check()?;
        validate_request(&req, &self.config)?;
        let resolver = CredentialResolver::new(self.store);
        let credential = resolver.resolve(
            self.config.profile.provider(),
            self.config.profile(),
            cancel,
        )?;
        let max_tokens = req
            .max_output_tokens()
            .unwrap_or_else(|| self.config.capabilities.max_output());
        let body = encode_anthropic_payload(&req, max_tokens, cancel)?;
        let encoded = serde_json::to_vec(&body).map_err(|_| ProviderError::InvalidRequest)?;
        if encoded.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        let url = self.config.endpoint.request_url();
        let request_id = req.request_id().as_str().to_owned();
        let mut parser = AnthropicSseTextDeltaParser::new();
        let headers = [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            (
                "anthropic-version".to_owned(),
                ANTHROPIC_API_VERSION.to_owned(),
            ),
            ("x-rapidlm-request-id".to_owned(), request_id),
        ];
        cancel.check()?;
        let outbound = ProviderHttpRequest::new(&url, &headers, &encoded, &credential);
        let response =
            self.transport
                .execute_streaming(&outbound, cancel, &mut |chunk: &str| {
                    parser.feed(chunk, on_text);
                })?;
        drop(credential);
        classify_http_error(&response)?;
        let events = parse_anthropic_stream(response.body(), cancel)?;
        ModelStream::from_events(
            req.request_id().clone(),
            req.model().clone(),
            events,
            cancel,
        )
    }
}

impl<T: HttpTransport> ProviderAdapter for AnthropicAdapter<'_, T> {
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

/// Convert a canonical request to the Anthropic Messages JSON object (no secrets).
pub fn encode_anthropic_payload(
    req: &CanonicalModelRequest,
    max_tokens: u32,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    cancel.check()?;
    if max_tokens == 0 {
        return Err(ProviderError::InvalidRequest);
    }

    let mut payload = Map::new();
    payload.insert(
        "model".to_owned(),
        Value::String(req.model().model().as_str().to_owned()),
    );
    payload.insert("stream".to_owned(), Value::Bool(true));
    payload.insert("max_tokens".to_owned(), Value::from(max_tokens));

    let (system, messages) = encode_messages(req.messages(), cancel)?;
    if messages.is_empty() {
        return Err(ProviderError::InvalidRequest);
    }
    if let Some(first) = messages.first()
        && first.get("role").and_then(Value::as_str) != Some("user")
    {
        return Err(ProviderError::InvalidRequest);
    }
    if let Some(system) = system {
        payload.insert("system".to_owned(), system);
    }
    payload.insert("messages".to_owned(), Value::Array(messages));

    if !req.tools().is_empty() {
        let mut tools = Vec::with_capacity(req.tools().len());
        for tool in req.tools() {
            tools.push(encode_tool(tool));
        }
        payload.insert("tools".to_owned(), Value::Array(tools));
    }
    Ok(Value::Object(payload))
}

fn validate_request(
    req: &CanonicalModelRequest,
    config: &AnthropicConfig,
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

fn encode_messages(
    messages: &[CanonicalMessage],
    cancel: &CancellationToken,
) -> Result<(Option<Value>, Vec<Value>), ProviderError> {
    let mut system_blocks = Vec::new();
    let mut wire: Vec<(MessageRole, Vec<Value>)> = Vec::new();

    for (i, message) in messages.iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check()?;
        }
        match message.role() {
            MessageRole::System => {
                system_blocks.extend(encode_text_blocks(message.parts())?);
            }
            MessageRole::User => {
                push_role_blocks(&mut wire, MessageRole::User, encode_user_blocks(message)?);
            }
            MessageRole::Assistant => {
                push_role_blocks(
                    &mut wire,
                    MessageRole::Assistant,
                    encode_assistant_blocks(message)?,
                );
            }
            MessageRole::Tool => {
                push_role_blocks(
                    &mut wire,
                    MessageRole::User,
                    encode_tool_result_blocks(message)?,
                );
            }
        }
    }

    let system = if system_blocks.is_empty() {
        None
    } else if system_blocks.len() == 1 {
        system_blocks[0]
            .get("text")
            .cloned()
            .or(Some(Value::Array(system_blocks)))
    } else {
        Some(Value::Array(system_blocks))
    };

    let mut encoded = Vec::with_capacity(wire.len());
    for (role, blocks) in wire {
        if blocks.is_empty() {
            return Err(ProviderError::InvalidRequest);
        }
        encoded.push(serde_json::json!({
            "role": role.as_str(),
            "content": blocks,
        }));
    }
    Ok((system, encoded))
}

fn push_role_blocks(
    wire: &mut Vec<(MessageRole, Vec<Value>)>,
    role: MessageRole,
    mut blocks: Vec<Value>,
) {
    if let Some((last_role, last_blocks)) = wire.last_mut()
        && *last_role == role
    {
        last_blocks.append(&mut blocks);
        return;
    }
    wire.push((role, blocks));
}

/// Split a `data:image/<type>;base64,<payload>` URL into its parts.
fn parse_data_url(data_url: &str) -> Result<(&'static str, &str), ProviderError> {
    const PNG: &str = "data:image/png;base64,";
    const JPEG: &str = "data:image/jpeg;base64,";
    const GIF: &str = "data:image/gif;base64,";
    const WEBP: &str = "data:image/webp;base64,";
    if let Some(data) = data_url.strip_prefix(PNG) {
        return Ok(("image/png", data));
    }
    if let Some(data) = data_url.strip_prefix(JPEG) {
        return Ok(("image/jpeg", data));
    }
    if let Some(data) = data_url.strip_prefix(GIF) {
        return Ok(("image/gif", data));
    }
    if let Some(data) = data_url.strip_prefix(WEBP) {
        return Ok(("image/webp", data));
    }
    Err(ProviderError::InvalidRequest)
}

fn encode_text_blocks(parts: &[ContentPart]) -> Result<Vec<Value>, ProviderError> {
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
            // Inline vision images ride tool results as base64 image blocks.
            ContentPart::ImageData { data_url } => {
                let (media_type, data) = parse_data_url(data_url)?;
                blocks.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    }
                }));
            }
            ContentPart::Image { .. } => return Err(ProviderError::InvalidRequest),
        }
    }
    Ok(blocks)
}

fn encode_user_blocks(message: &CanonicalMessage) -> Result<Vec<Value>, ProviderError> {
    if !message.tool_calls().is_empty() {
        return Err(ProviderError::InvalidRequest);
    }
    let mut blocks = Vec::new();
    for part in message.parts() {
        match part {
            ContentPart::Text { text } => {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
            ContentPart::ImageData { data_url } => {
                let (media_type, data) = parse_data_url(data_url)?;
                blocks.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    }
                }));
            }
            ContentPart::Image { artifact } => {
                blocks.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "url",
                        "url": artifact_url(&artifact.id),
                        "media_type": artifact.media_type,
                    }
                }));
            }
        }
    }
    Ok(blocks)
}

fn encode_assistant_blocks(message: &CanonicalMessage) -> Result<Vec<Value>, ProviderError> {
    let mut blocks = encode_text_blocks(message.parts())?;
    for call in message.tool_calls() {
        let input = parse_tool_input(call.arguments())?;
        blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": call.call_id().as_str(),
            "name": call.name().as_str(),
            "input": input,
        }));
    }
    Ok(blocks)
}

fn encode_tool_result_blocks(message: &CanonicalMessage) -> Result<Vec<Value>, ProviderError> {
    let call_id = message
        .tool_call_id()
        .ok_or(ProviderError::InvalidRequest)?;
    Ok(vec![serde_json::json!({
        "type": "tool_result",
        "tool_use_id": call_id.as_str(),
        "content": flatten_text_parts(message.parts()),
    })])
}

fn encode_tool(tool: &CanonicalToolSpec) -> Value {
    serde_json::json!({
        "name": tool.name().as_str(),
        "description": tool.description(),
        "input_schema": tool.parameters(),
    })
}

fn parse_tool_input(arguments: &str) -> Result<Value, ProviderError> {
    let value: Value =
        serde_json::from_str(arguments).map_err(|_| ProviderError::InvalidRequest)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(ProviderError::InvalidRequest)
    }
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

fn classify_http_error(
    response: &crate::providers::openai_compatible::ProviderHttpResponse,
) -> Result<(), ProviderError> {
    if (200..300).contains(&response.status()) {
        return Ok(());
    }
    let parsed = parse_json_object(response.body());
    match response.status() {
        401 | 403 => Err(ProviderError::AuthFailed),
        402 => Err(ProviderError::QuotaExceeded),
        429 => Err(ProviderError::RateLimited {
            retry_after_ms: response
                .header("retry-after")
                .and_then(parse_retry_after_ms),
        }),
        400 | 413 if parsed.as_ref().is_some_and(json_is_context_too_large) => {
            Err(ProviderError::ContextTooLarge)
        }
        408 | 409 | 425 | 500 | 502 | 503 | 504 | 529 => Err(ProviderError::Transient),
        // A redirect is never followed, and asking again is redirected again.
        300..=499 => Err(ProviderError::Permanent),
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
    let code = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let haystack = format!("{code} {message}").to_ascii_lowercase();
    haystack.contains("prompt is too long")
        || haystack.contains("context_length_exceeded")
        || haystack.contains("context length")
        || haystack.contains("maximum context")
        || haystack.contains("too many tokens")
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

/// Incremental SSE parser for Anthropic Messages streams: forwards
/// `content_block_delta` text deltas to `on_text` as their frames complete,
/// retaining only the incomplete frame across chunks. Framing matches the
/// OpenAI-compatible parser (blank-line-separated `data:` lines); the delta
/// JSON shape is Anthropic's.
#[derive(Default)]
pub struct AnthropicSseTextDeltaParser {
    buffer: String,
}

impl AnthropicSseTextDeltaParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed newly arrived text; every complete frame's text delta (if any)
    /// is forwarded to `on_text`. Returns the number of deltas emitted.
    pub fn feed(&mut self, chunk: &str, on_text: &mut dyn FnMut(&str)) -> usize {
        self.buffer.push_str(chunk);
        let mut emitted = 0usize;
        while let Some(end) = self.buffer.find("\n\n") {
            let block = self.buffer[..end].to_string();
            self.buffer.drain(..end + 2);
            let mut data = String::new();
            for line in block.lines() {
                let line = line.trim_end_matches('\r');
                if let Some(payload) = line.strip_prefix("data:") {
                    let payload = payload.strip_prefix(' ').unwrap_or(payload);
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(payload);
                }
            }
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let is_text_delta = value.get("type").and_then(Value::as_str)
                == Some("content_block_delta")
                && value
                    .get("delta")
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str)
                    == Some("text_delta");
            if !is_text_delta {
                continue;
            }
            if let Some(text) = value
                .get("delta")
                .and_then(|delta| delta.get("text"))
                .and_then(Value::as_str)
            {
                on_text(text);
                emitted += 1;
            }
        }
        emitted
    }
}

fn parse_anthropic_stream(
    body: &[u8],
    cancel: &CancellationToken,
) -> Result<Vec<ModelStreamEvent>, ProviderError> {
    cancel.check()?;
    let text = std::str::from_utf8(body).map_err(|_| ProviderError::Permanent)?;
    let mut events = Vec::new();
    let mut tool_ids: BTreeMap<u32, ToolCallId> = BTreeMap::new();
    let mut finish = None;
    let mut usage = NormalizedUsage::new(None, None, None, None, None, None, UsageCost::Unknown);
    let mut saw_data = false;
    let mut saw_terminal = false;

    for (i, block) in sse_data_blocks(text).into_iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check()?;
        }
        if block == "[DONE]" {
            saw_terminal = true;
            break;
        }
        let value: Value = serde_json::from_str(&block).map_err(|_| ProviderError::Permanent)?;
        if is_error_event(&value) {
            return map_in_stream_error(&value);
        }
        saw_data = true;
        ingest_event(
            &value,
            &mut events,
            &mut tool_ids,
            &mut finish,
            &mut usage,
            &mut saw_terminal,
            cancel,
        )?;
        if events.len() > MAX_STREAM_EVENTS {
            return Err(ProviderError::BoundExceeded);
        }
    }

    if !saw_data {
        if let Some(value) = parse_json_object(body) {
            if is_error_event(&value) {
                return map_in_stream_error(&value);
            }
            ingest_non_stream_message(&value, &mut events, &mut finish, &mut usage, cancel)?;
        } else {
            return Err(ProviderError::Permanent);
        }
    } else if finish.is_none() && !saw_terminal {
        return Err(ProviderError::Permanent);
    }

    let finish = finish.unwrap_or_else(|| infer_finish(&events));
    push_event(&mut events, ModelStreamEvent::Completed { finish, usage })?;
    Ok(events)
}

fn is_error_event(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("error") || value.get("error").is_some()
}

fn map_in_stream_error(value: &Value) -> Result<Vec<ModelStreamEvent>, ProviderError> {
    if json_is_context_too_large(value) {
        return Err(ProviderError::ContextTooLarge);
    }
    let error = value.get("error").unwrap_or(value);
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if kind.contains("auth") || kind.contains("permission") {
        return Err(ProviderError::AuthFailed);
    }
    if kind.contains("rate") {
        return Err(ProviderError::RateLimited {
            retry_after_ms: None,
        });
    }
    if kind.contains("overloaded") || kind == "api_error" {
        return Err(ProviderError::Transient);
    }
    Err(ProviderError::Permanent)
}

fn ingest_event(
    value: &Value,
    events: &mut Vec<ModelStreamEvent>,
    tool_ids: &mut BTreeMap<u32, ToolCallId>,
    finish: &mut Option<FinishReason>,
    usage: &mut NormalizedUsage,
    saw_terminal: &mut bool,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    cancel.check()?;
    let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "message_start" => {
            if let Some(raw) = value
                .pointer("/message/usage")
                .filter(|item| !item.is_null())
            {
                *usage = merge_usage(usage, normalize_anthropic_usage(raw)?);
                push_event(events, ModelStreamEvent::Usage(usage.clone()))?;
            }
        }
        "content_block_start" => ingest_content_block_start(value, events, tool_ids)?,
        "content_block_delta" => ingest_content_block_delta(value, events, tool_ids)?,
        "message_delta" => {
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                *finish = Some(map_stop_reason(reason)?);
            }
            if let Some(raw) = value.get("usage").filter(|item| !item.is_null()) {
                *usage = merge_usage(usage, normalize_anthropic_usage(raw)?);
                push_event(events, ModelStreamEvent::Usage(usage.clone()))?;
            }
        }
        "message_stop" => {
            *saw_terminal = true;
            if finish.is_none() {
                *finish = Some(infer_finish(events));
            }
        }
        "ping" | "content_block_stop" => {}
        _ => {
            if let Some(text) = value.pointer("/delta/text").and_then(Value::as_str) {
                push_text_deltas(events, text)?;
            }
        }
    }
    Ok(())
}

fn ingest_content_block_start(
    value: &Value,
    events: &mut Vec<ModelStreamEvent>,
    tool_ids: &mut BTreeMap<u32, ToolCallId>,
) -> Result<(), ProviderError> {
    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
    let block = value.get("content_block").unwrap_or(&Value::Null);
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "tool_use" {
        let call_id = block
            .get("id")
            .and_then(Value::as_str)
            .ok_or(ProviderError::Permanent)?;
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .ok_or(ProviderError::Permanent)?;
        let parsed_id = ToolCallId::parse(call_id)?;
        let parsed_name = ToolName::parse(name)?;
        tool_ids.insert(index, parsed_id.clone());
        push_event(
            events,
            ModelStreamEvent::ToolCallStart {
                call_id: parsed_id.clone(),
                name: parsed_name,
            },
        )?;
        if let Some(input) = block.get("input")
            && input.is_object()
            && input.as_object().is_some_and(|object| !object.is_empty())
        {
            let encoded = serde_json::to_string(input).map_err(|_| ProviderError::Permanent)?;
            push_argument_deltas(events, parsed_id, &encoded)?;
        }
    } else if kind == "text"
        && let Some(text) = block.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        push_text_deltas(events, text)?;
    }
    Ok(())
}

fn ingest_content_block_delta(
    value: &Value,
    events: &mut Vec<ModelStreamEvent>,
    tool_ids: &BTreeMap<u32, ToolCallId>,
) -> Result<(), ProviderError> {
    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
    let delta = value.get("delta").unwrap_or(&Value::Null);
    let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text_delta" => {
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                push_text_deltas(events, text)?;
            }
        }
        "input_json_delta" => {
            let call_id = tool_ids
                .get(&index)
                .cloned()
                .ok_or(ProviderError::Permanent)?;
            if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                push_argument_deltas(events, call_id, partial)?;
            }
        }
        _ => {
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                push_text_deltas(events, text)?;
            }
        }
    }
    Ok(())
}

fn ingest_non_stream_message(
    value: &Value,
    events: &mut Vec<ModelStreamEvent>,
    finish: &mut Option<FinishReason>,
    usage: &mut NormalizedUsage,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    cancel.check()?;
    if let Some(raw) = value.get("usage").filter(|item| !item.is_null()) {
        *usage = normalize_anthropic_usage(raw)?;
        push_event(events, ModelStreamEvent::Usage(usage.clone()))?;
    }
    if let Some(reason) = value.get("stop_reason").and_then(Value::as_str) {
        *finish = Some(map_stop_reason(reason)?);
    }
    let content = value.get("content").and_then(Value::as_array);
    if let Some(blocks) = content {
        for block in blocks {
            let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        push_text_deltas(events, text)?;
                    }
                }
                "tool_use" => {
                    let call_id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or(ProviderError::Permanent)?;
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or(ProviderError::Permanent)?;
                    let parsed_id = ToolCallId::parse(call_id)?;
                    push_event(
                        events,
                        ModelStreamEvent::ToolCallStart {
                            call_id: parsed_id.clone(),
                            name: ToolName::parse(name)?,
                        },
                    )?;
                    if let Some(input) = block.get("input") {
                        let encoded = if input.is_string() {
                            input.as_str().unwrap_or("").to_owned()
                        } else {
                            serde_json::to_string(input).map_err(|_| ProviderError::Permanent)?
                        };
                        if !encoded.is_empty() && encoded != "{}" {
                            push_argument_deltas(events, parsed_id, &encoded)?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if finish.is_none() {
        *finish = Some(infer_finish(events));
    }
    Ok(())
}

fn normalize_anthropic_usage(value: &Value) -> Result<NormalizedUsage, ProviderError> {
    let object = value.as_object().ok_or(ProviderError::InvalidRequest)?;
    let raw_input = first_u64(object, &["input_tokens"])?;
    let output = first_u64(object, &["output_tokens"])?;
    let cache_read = first_u64(object, &["cache_read_input_tokens"])?;
    let cache_create = first_u64(object, &["cache_creation_input_tokens"])?;
    let cached = cache_read;
    let uncached = match (raw_input, cache_create) {
        (Some(input), Some(created)) => Some(input.saturating_add(created)),
        (Some(input), None) => Some(input),
        (None, Some(created)) => Some(created),
        (None, None) => None,
    };
    let input = match (uncached, cached) {
        (Some(uncached), Some(cached)) => Some(uncached.saturating_add(cached)),
        (Some(uncached), None) => Some(uncached),
        (None, Some(cached)) => Some(cached),
        (None, None) => None,
    };
    let cost = if let Some(micros) = first_u64(object, &["cost_usd_micros"])? {
        UsageCost::Reported { usd_micros: micros }
    } else {
        UsageCost::Unknown
    };
    let mut usage = NormalizedUsage::new(input, cached, uncached, output, None, None, cost);
    if let Some(created) = cache_create {
        usage.insert_extra("cache_creation_input_tokens", UsageExtValue::U64(created))?;
    }
    if let Some(read) = cache_read {
        usage.insert_extra("cache_read_input_tokens", UsageExtValue::U64(read))?;
    }
    Ok(usage)
}

fn merge_usage(current: &NormalizedUsage, incoming: NormalizedUsage) -> NormalizedUsage {
    let mut merged = NormalizedUsage::new(
        incoming.input_tokens().or(current.input_tokens()),
        incoming
            .cached_input_tokens()
            .or(current.cached_input_tokens()),
        incoming
            .uncached_input_tokens()
            .or(current.uncached_input_tokens()),
        incoming.output_tokens().or(current.output_tokens()),
        incoming.reasoning_tokens().or(current.reasoning_tokens()),
        incoming.tool_tokens().or(current.tool_tokens()),
        if incoming.cost() == UsageCost::Unknown {
            current.cost()
        } else {
            incoming.cost()
        },
    );
    for (key, value) in current.extra_fields() {
        let _ = merged.insert_extra(key, value.clone());
    }
    for (key, value) in incoming.extra_fields() {
        let _ = merged.insert_extra(key, value.clone());
    }
    merged
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

fn map_stop_reason(raw: &str) -> Result<FinishReason, ProviderError> {
    match raw {
        "end_turn" | "stop_sequence" | "pause_turn" => Ok(FinishReason::Stop),
        "max_tokens" => Ok(FinishReason::Length),
        "tool_use" => Ok(FinishReason::ToolCalls),
        "refusal" => Err(ProviderError::Permanent),
        "" | "null" => Ok(FinishReason::Stop),
        _ => Err(ProviderError::Permanent),
    }
}

fn infer_finish(events: &[ModelStreamEvent]) -> FinishReason {
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
    for chunk in bounded_chunks(arguments)? {
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

impl Debug for AnthropicConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("profile", &self.profile)
            .field("endpoint", &self.endpoint)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl Debug for AnthropicEndpoint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicEndpoint")
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl<T> Debug for AnthropicAdapter<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicAdapter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for AnthropicEndpoint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::Mutex;

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
    use crate::providers::openai_compatible::ProviderHttpResponse;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-9f3c2a";
    const REF_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const TRACE: &str = "0193e0f6-6c3a-7d1e-8b2c-4d5e6f708192";

    struct CapturedRequest {
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    struct ScriptedTransport {
        status: u16,
        body: Vec<u8>,
        extra_headers: Vec<(String, String)>,
        captured: Mutex<Option<CapturedRequest>>,
    }

    impl ScriptedTransport {
        fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                body: body.into(),
                extra_headers: Vec::new(),
                captured: Mutex::new(None),
            }
        }

        fn with_retry_after(status: u16, body: impl Into<Vec<u8>>, retry_after: &str) -> Self {
            Self {
                status,
                body: body.into(),
                extra_headers: vec![("retry-after".to_owned(), retry_after.to_owned())],
                captured: Mutex::new(None),
            }
        }

        fn captured(&self) -> CapturedRequest {
            let guard = self.captured.lock().expect("lock");
            let captured = guard.as_ref().expect("captured request");
            CapturedRequest {
                url: captured.url.clone(),
                headers: captured.headers.clone(),
                body: captured.body.clone(),
            }
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn execute(
            &self,
            request: &ProviderHttpRequest<'_>,
            cancel: &CancellationToken,
        ) -> Result<ProviderHttpResponse, ProviderError> {
            cancel.check()?;
            let captured = CapturedRequest {
                url: request.url().to_owned(),
                headers: request.headers().to_vec(),
                body: String::from_utf8_lossy(request.body()).into_owned(),
            };
            assert!(
                !captured.body.contains(CANARY),
                "request body leaked credential plaintext"
            );
            *self.captured.lock().expect("lock") = Some(captured);
            ProviderHttpResponse::new(self.status, self.extra_headers.clone(), self.body.clone())
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn anthropic() -> ProviderId {
        ProviderId::parse("anthropic").expect("provider")
    }

    fn sample_ref() -> SecretRef {
        SecretRef::from_id_and_alias(REF_ID, "env:ANTHROPIC_API_KEY").expect("ref")
    }

    fn caps(tools: bool, vision: bool) -> ProviderCapabilities {
        ProviderCapabilities::new(
            tools,
            true,
            vision,
            true,
            ReasoningSupport::Exposed,
            false,
            200_000,
            8192,
            UsageFieldSet::new(true, true, true, true, false, false, false),
        )
        .expect("caps")
    }

    fn store_with_canary() -> InMemoryCredentialStore {
        let store = InMemoryCredentialStore::new();
        let value = SecretValue::from_str(sample_ref(), CANARY).expect("secret");
        let put = CredentialPut::ephemeral(value, CredentialKind::ProviderApiKey)
            .with_provider("anthropic")
            .expect("provider");
        store
            .put(put, &auth::CancellationToken::new())
            .expect("put");
        store
    }

    fn profile() -> ProviderProfile {
        ProviderProfile::new("default", anthropic(), sample_ref()).expect("profile")
    }

    fn config(base: &str, capabilities: ProviderCapabilities) -> AnthropicConfig {
        AnthropicConfig::new(
            profile(),
            AnthropicEndpoint::new(base).expect("endpoint"),
            capabilities,
        )
        .expect("config")
    }

    fn adapter<'a>(
        store: &'a InMemoryCredentialStore,
        transport: ScriptedTransport,
        capabilities: ProviderCapabilities,
    ) -> AnthropicAdapter<'a, ScriptedTransport> {
        AnthropicAdapter::new(
            config("http://127.0.0.1:9/v1", capabilities),
            transport,
            store,
        )
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
            ModelRequestId::parse("req-anthropic-1").expect("id"),
            ModelRef::new(
                anthropic(),
                ModelId::parse("claude-3-5-sonnet").expect("model"),
            ),
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
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":8,\"cache_read_input_tokens\":4,\"cache_creation_input_tokens\":2,\"output_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n"
            .to_owned()
    }

    fn sse_tools() -> String {
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":8,\"output_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\",\"input\":{}}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":1}\"}}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":2}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n"
            .to_owned()
    }

    /// Transport whose streaming path delivers the body in fixed-size
    /// slices BEFORE returning the canonical response — mid-frame splits
    /// included, which is the case the default (delegate) transport can
    /// never exercise.
    struct SlicedStreamingTransport {
        inner: ScriptedTransport,
        slice: usize,
    }
    impl HttpTransport for SlicedStreamingTransport {
        fn execute(
            &self,
            request: &ProviderHttpRequest<'_>,
            cancel: &CancellationToken,
        ) -> Result<ProviderHttpResponse, ProviderError> {
            self.inner.execute(request, cancel)
        }
        fn execute_streaming(
            &self,
            request: &ProviderHttpRequest<'_>,
            cancel: &CancellationToken,
            on_body: &mut dyn FnMut(&str),
        ) -> Result<ProviderHttpResponse, ProviderError> {
            let response = self.inner.execute(request, cancel)?;
            let body = String::from_utf8_lossy(response.body()).into_owned();
            for chunk in body.as_bytes().chunks(self.slice) {
                on_body(&String::from_utf8_lossy(chunk));
            }
            Ok(response)
        }
    }

    #[test]
    fn streaming_forwards_text_deltas_progressively_and_canonical_parse_holds() {
        let body = sse_text();
        let transport = SlicedStreamingTransport {
            inner: ScriptedTransport::new(200, body.into_bytes()),
            slice: 41, // deliberately mid-frame
        };
        let store = store_with_canary();
        let adapter = AnthropicAdapter::new(
            config("http://127.0.0.1:9/v1", caps(true, false)),
            transport,
            &store,
        );
        let req = request(false, false);
        let deltas = std::sync::Arc::<std::sync::Mutex<Vec<String>>>::default();
        let sink = deltas.clone();
        let stream = adapter
            .invoke_sync_streaming(req, &live(), &mut |text| {
                sink.lock().expect("lock").push(text.to_owned())
            })
            .expect("streaming invoke");
        // Deltas surfaced live, in order, exactly as the frames carried.
        assert_eq!(deltas.lock().expect("lock").as_slice(), ["Hello", " world"]);
        // The canonical events are the buffered path's: full text + Completed.
        let mut text = String::new();
        let mut completed = false;
        for event in stream.events() {
            if let ModelStreamEvent::TextDelta { text: chunk } = event {
                text.push_str(chunk);
            }
            if matches!(event, ModelStreamEvent::Completed { .. }) {
                completed = true;
            }
        }
        assert_eq!(text, "Hello world");
        assert!(completed, "canonical Completed event present");
    }

    #[test]
    fn delta_parser_ignores_non_text_frames_and_survives_split_frames() {
        let mut parser = AnthropicSseTextDeltaParser::new();
        let mut seen = Vec::new();
        // Split the frame boundary inside the JSON payload.
        let first = "event: content_block_delta\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text";
        let second = "_delta\",\"text\":\"hi\"}}\n\n";
        parser.feed(first, &mut |text| seen.push(text.to_owned()));
        assert!(seen.is_empty(), "incomplete frame emits nothing");
        parser.feed(second, &mut |text| seen.push(text.to_owned()));
        assert_eq!(seen, ["hi"]);
        // Non-text shapes emit nothing.
        let before = seen.len();
        parser.feed(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            &mut |text| seen.push(text.to_owned()),
        );
        parser.feed("data: {\"type\":\"ping\"}\n\n", &mut |text| {
            seen.push(text.to_owned())
        });
        assert_eq!(seen.len(), before);
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
    fn messages_stream_emits_normalized_text_and_cache_usage() {
        let store = store_with_canary();
        let transport = ScriptedTransport::new(200, sse_text());
        let adapter = adapter(&store, transport, caps(false, false));
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
        assert_eq!(usage.input_tokens(), Some(14));
        assert_eq!(usage.cached_input_tokens(), Some(4));
        assert_eq!(usage.uncached_input_tokens(), Some(10));
        assert_eq!(usage.output_tokens(), Some(3));
        assert_eq!(usage.cost(), UsageCost::Unknown);
        assert_ne!(usage.cost(), UsageCost::Reported { usd_micros: 0 });
        assert_eq!(
            usage.extra("cache_creation_input_tokens"),
            Some(&UsageExtValue::U64(2))
        );

        let captured = adapter.transport.captured();
        assert!(captured.url.ends_with("/v1/messages"));
        assert!(
            captured
                .headers
                .iter()
                .any(|(k, v)| k == "anthropic-version" && v == ANTHROPIC_API_VERSION)
        );
        assert!(!captured.body.contains(CANARY));
        let payload: Value = serde_json::from_str(&captured.body).expect("json");
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["model"], "claude-3-5-sonnet");
        assert_eq!(payload["max_tokens"], 256);
        assert_eq!(payload["system"], "sys");
        assert_eq!(payload["messages"][0]["role"], "user");
        assert!(
            payload
                .get("messages")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "system")
        );
    }

    #[test]
    fn messages_stream_preserves_tool_order_and_call_deltas() {
        let store = store_with_canary();
        let transport = ScriptedTransport::new(200, sse_tools());
        let adapter = adapter(&store, transport, caps(true, false));
        let req = request(true, false);
        let encoded = encode_anthropic_payload(&req, 256, &live()).expect("encode");
        assert_eq!(encoded["tools"][0]["name"], "search");
        assert_eq!(encoded["tools"][1]["name"], "read");
        assert_eq!(encoded["tools"][0]["input_schema"]["type"], "object");
        assert!(encoded["tools"][0].get("function").is_none());
        let stream = block_on(adapter.invoke(req, live())).expect("invoke");
        let mut saw_start = false;
        let mut args = String::new();
        let mut finish = None;
        for event in stream.events() {
            match event {
                ModelStreamEvent::ToolCallStart { call_id, name } => {
                    assert_eq!(call_id.as_str(), "toolu_1");
                    assert_eq!(name.as_str(), "search");
                    saw_start = true;
                }
                ModelStreamEvent::ToolCallArgumentsDelta {
                    call_id,
                    arguments_delta,
                } => {
                    assert_eq!(call_id.as_str(), "toolu_1");
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
    fn rate_limit_and_auth_and_context_map_to_typed_errors() {
        let store = store_with_canary();

        let limited = ScriptedTransport::with_retry_after(
            429,
            format!(
                r#"{{"type":"error","error":{{"type":"rate_limit_error","message":"rate {CANARY}"}}}}"#
            ),
            "2",
        );
        let err = block_on(
            adapter(&store, limited, caps(false, false)).invoke(request(false, false), live()),
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

        let auth = ScriptedTransport::new(
            401,
            format!(
                r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key: {CANARY}"}}}}"#
            ),
        );
        let err = block_on(
            adapter(&store, auth, caps(false, false)).invoke(request(false, false), live()),
        )
        .expect_err("auth");
        assert_eq!(err, ProviderError::AuthFailed);
        assert_eq!(err.code(), Some(ErrorCode::ProviderAuthFailed));
        assert!(!err.is_retryable());
        assert_no_canary("auth", &format!("{err:?}{err}"));

        let too_big = ScriptedTransport::new(
            400,
            format!(
                r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"prompt is too long {CANARY}"}}}}"#
            ),
        );
        let err = block_on(
            adapter(&store, too_big, caps(false, false)).invoke(request(false, false), live()),
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
        let transport = ScriptedTransport::new(
            403,
            format!(
                r#"{{"type":"error","error":{{"type":"permission_error","message":"{CANARY}"}}}}"#
            ),
        );
        let err = block_on(
            adapter(&store, transport, caps(false, false)).invoke(request(false, false), live()),
        )
        .expect_err("forbidden");
        assert_eq!(err, ProviderError::AuthFailed);
        assert_ne!(err, ProviderError::Transient);
        assert!(!err.is_retryable());
        assert_no_canary("forbidden", &format!("{err}"));
    }

    #[test]
    fn payment_required_is_an_exhausted_quota_never_retried() {
        let store = store_with_canary();
        let transport = ScriptedTransport::new(
            402,
            format!(
                r#"{{"type":"error","error":{{"type":"billing_error","message":"{CANARY}"}}}}"#
            ),
        );
        let err = block_on(
            adapter(&store, transport, caps(false, false)).invoke(request(false, false), live()),
        )
        .expect_err("payment required");
        assert_eq!(err, ProviderError::QuotaExceeded);
        assert!(!err.is_retryable());
        assert_no_canary("quota", &format!("{err}"));
    }

    #[test]
    fn missing_credential_is_auth_failed_without_network() {
        let store = InMemoryCredentialStore::new();
        let transport = ScriptedTransport::new(200, sse_text());
        let adapter = AnthropicAdapter::new(
            config("http://127.0.0.1:1/v1", caps(false, false)),
            transport,
            &store,
        );
        let err = adapter
            .invoke_sync(request(false, false), &live())
            .expect_err("missing");
        assert_eq!(err, ProviderError::AuthFailed);
        assert_no_canary("missing", &format!("{err:?}"));
        assert!(adapter.transport.captured.lock().expect("lock").is_none());
    }

    #[test]
    fn cancellation_is_not_swallowed() {
        let store = store_with_canary();
        let transport = ScriptedTransport::new(200, sse_text());
        let adapter = AnthropicAdapter::new(
            config("http://127.0.0.1:1/v1", caps(false, false)),
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
            AnthropicEndpoint::new("http://169.254.169.254/v1").expect_err("metadata"),
            ProviderError::InvalidRequest
        );
        assert_eq!(
            AnthropicEndpoint::new(format!("http://user:{CANARY}@127.0.0.1/v1"))
                .expect_err("userinfo"),
            ProviderError::InvalidRequest
        );
        assert!(AnthropicEndpoint::new("https://api.anthropic.com/v1").is_ok());
    }

    #[test]
    fn image_parts_are_artifact_refs_not_bytes() {
        let req = request(false, true);
        let encoded = encode_anthropic_payload(&req, 256, &live()).expect("encode");
        let content = &encoded["messages"][0]["content"];
        assert_eq!(content[1]["type"], "image");
        let url = content[1]["source"]["url"].as_str().expect("url");
        assert!(url.starts_with("artifact:sha256:"));
        assert!(!url.contains("PNG"));
        assert_eq!(content[1].get("bytes"), None);
        assert_eq!(content[1]["source"].get("data"), None);
    }

    #[test]
    fn adapter_debug_redacts_credential_plaintext() {
        let store = store_with_canary();
        let transport = ScriptedTransport::new(200, sse_text());
        let adapter = AnthropicAdapter::new(
            config("http://127.0.0.1:9/v1", caps(false, false)),
            transport,
            &store,
        );
        assert_no_canary("adapter debug", &format!("{adapter:?}"));
        assert_no_canary("config debug", &format!("{:?}", adapter.config()));
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
            ModelRef::new(
                anthropic(),
                ModelId::parse("claude-3-5-sonnet").expect("model"),
            ),
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
        let encoded = encode_anthropic_payload(&req, 128, &live()).expect("encode");
        assert!(encoded.get("system").is_none());
        assert_eq!(encoded["messages"][0]["role"], "user");
        assert_eq!(encoded["messages"][1]["role"], "assistant");
        assert_eq!(encoded["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(encoded["messages"][1]["content"][0]["id"], "call_a");
        assert_eq!(encoded["messages"][1]["content"][1]["id"], "call_b");
        assert_eq!(encoded["messages"][2]["role"], "user");
        assert_eq!(encoded["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(
            encoded["messages"][2]["content"][0]["tool_use_id"],
            "call_a"
        );
        assert_eq!(
            encoded["messages"][2]["content"][1]["tool_use_id"],
            "call_b"
        );
        assert_eq!(encoded["max_tokens"], 128);
    }

    #[test]
    fn truncated_stream_without_terminal_is_rejected() {
        let prefix = "event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n";
        let store = store_with_canary();
        let transport = ScriptedTransport::new(200, prefix);
        let err = adapter(&store, transport, caps(false, false))
            .invoke_sync(request(false, false), &live())
            .expect_err("prefix");
        assert_eq!(err, ProviderError::Permanent);
        assert_ne!(err, ProviderError::AuthFailed);
    }

    #[test]
    fn in_stream_error_discards_body() {
        let store = store_with_canary();
        let body = format!(
            "event: error\n\
             data: {{\"type\":\"error\",\"error\":{{\"type\":\"authentication_error\",\"message\":\"{CANARY}\"}}}}\n\n"
        );
        let transport = ScriptedTransport::new(200, body);
        let err = adapter(&store, transport, caps(false, false))
            .invoke_sync(request(false, false), &live())
            .expect_err("stream auth");
        assert_eq!(err, ProviderError::AuthFailed);
        assert_no_canary("stream auth", &format!("{err:?}{err}"));
    }

    #[test]
    fn credential_resolver_is_used_before_transport() {
        let store = store_with_canary();
        let resolver = CredentialResolver::new(&store);
        let resolved = resolver
            .resolve(&anthropic(), &profile(), &live())
            .expect("resolve");
        assert_eq!(resolved.byte_len(), CANARY.len());
        assert_no_canary("resolved", &format!("{resolved:?}"));
    }

    #[test]
    fn a_redirect_is_not_retried() {
        for status in [301, 308] {
            let response = crate::providers::openai_compatible::ProviderHttpResponse::new(
                status,
                Vec::new(),
                Vec::new(),
            )
            .expect("response");
            assert_eq!(
                classify_http_error(&response),
                Err(ProviderError::Permanent),
                "{status}"
            );
        }
    }
}
