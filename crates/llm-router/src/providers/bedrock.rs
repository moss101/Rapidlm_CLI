//! Native Bedrock ConverseStream over shared HTTP, auth and private replay contracts.
use super::{
    aws_eventstream::{EventFrame, FrameDecoder},
    aws_sigv4::uri_encode,
    openai_compatible::{
        HttpTransport, MAX_HTTP_REQUEST_BYTES, OpenAiApiStyle, OpenAiCompatibleEndpoint,
        ProviderAuthStyle, ProviderHttpRequest, classify_http_error, push_argument_deltas,
        push_event, push_text_deltas,
    },
};
use crate::{
    CancellationToken, CanonicalModelRequest, ContentPart, CredentialResolver, FinishReason,
    MessageRole, ModelStream, ModelStreamEvent, NormalizedUsage, ProviderAdapter,
    ProviderCapabilities, ProviderError, ProviderProfile, ToolCallId, ToolName, UsageCost,
    UsageExtValue,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use protocol::provider_replay::{
    MAX_REPLAY_BYTES, MAX_REPLAY_ITEMS, ProviderReplayState, ReplayData, ReplayItem,
    ReplayProtocol, scope_key,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BedrockEndpoint {
    base_url: String,
    region: String,
    resource: Option<String>,
    signed: bool,
}
impl BedrockEndpoint {
    pub fn new(base_url: &str, region: &str, signed: bool) -> Result<Self, ProviderError> {
        if region.is_empty()
            || region.len() > 64
            || !region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ProviderError::InvalidRequest);
        }
        let checked = OpenAiCompatibleEndpoint::new(base_url, OpenAiApiStyle::ChatCompletions)?;
        Ok(Self {
            base_url: checked.base_url().to_owned(),
            region: region.to_owned(),
            resource: None,
            signed,
        })
    }
    pub fn with_resource(mut self, resource: &str) -> Result<Self, ProviderError> {
        validate_resource(resource)?;
        self.resource = Some(resource.to_owned());
        Ok(self)
    }
    pub fn region(&self) -> &str {
        &self.region
    }
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    pub fn signed(&self) -> bool {
        self.signed
    }
    pub fn replay_endpoint(&self, model: &str) -> Result<String, ProviderError> {
        Ok(format!(
            "{}|region={}|auth={}",
            self.request_url(model)?,
            self.region,
            if self.signed { "sigv4" } else { "bearer" }
        ))
    }
    pub fn request_url(&self, model: &str) -> Result<String, ProviderError> {
        let resource = self.resource.as_deref().unwrap_or(model);
        validate_resource(resource)?;
        Ok(format!(
            "{}/model/{}/converse-stream",
            self.base_url.trim_end_matches('/'),
            uri_encode(resource.as_bytes(), false)
        ))
    }
}
fn native_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn validate_resource(resource: &str) -> Result<(), ProviderError> {
    if resource.is_empty()
        || resource.len() > 2048
        || !resource
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/@".contains(&b))
        || resource.split('/').any(|p| matches!(p, "." | ".."))
    {
        return Err(ProviderError::InvalidRequest);
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BedrockEffortMode {
    #[default]
    Unspecified,
    Anthropic,
    Nova,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BedrockConfig {
    profile: ProviderProfile,
    endpoint: BedrockEndpoint,
    capabilities: ProviderCapabilities,
    additional_fields: Option<Value>,
    effort_mode: BedrockEffortMode,
    thinking: Option<super::anthropic::AnthropicThinking>,
}
impl BedrockConfig {
    pub fn new(
        profile: ProviderProfile,
        endpoint: BedrockEndpoint,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ProviderError> {
        if !capabilities.streaming() {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            profile,
            endpoint,
            capabilities,
            additional_fields: None,
            effort_mode: BedrockEffortMode::Unspecified,
            thinking: None,
        })
    }
    pub fn with_effort_mode(mut self, mode: BedrockEffortMode) -> Self {
        self.effort_mode = mode;
        self
    }
    pub fn with_thinking(
        mut self,
        thinking: super::anthropic::AnthropicThinking,
    ) -> Result<Self, ProviderError> {
        if self.effort_mode != BedrockEffortMode::Anthropic
            || self.capabilities.reasoning() == crate::ReasoningSupport::None
        {
            return Err(ProviderError::InvalidRequest);
        }
        if let super::anthropic::AnthropicThinking::Enabled { budget_tokens } = thinking
            && (budget_tokens < 1024 || budget_tokens >= self.capabilities.max_output())
        {
            return Err(ProviderError::InvalidRequest);
        }
        self.thinking = Some(thinking);
        Ok(self)
    }
    /// Trusted configuration only. Native model-specific parameters stay in their
    /// dedicated field and cannot replace messages, origin, tools or credentials.
    pub fn with_additional_fields(mut self, fields: Value) -> Result<Self, ProviderError> {
        if !fields.is_object()
            || serde_json::to_vec(&fields)
                .map_err(|_| ProviderError::InvalidRequest)?
                .len()
                > 16 * 1024
        {
            return Err(ProviderError::InvalidRequest);
        }
        if [
            "messages",
            "system",
            "model",
            "modelId",
            "tools",
            "toolConfig",
            "stream",
            "max_tokens",
            "maxTokens",
        ]
        .iter()
        .any(|key| fields.get(key).is_some())
            || fields.get("inferenceConfig").is_some_and(|value| {
                value.get("maxTokens").is_some() || value.get("max_tokens").is_some()
            })
        {
            return Err(ProviderError::InvalidRequest);
        }
        if fields
            .get("output_config")
            .is_some_and(|value| !value.is_object())
        {
            return Err(ProviderError::InvalidRequest);
        }
        self.additional_fields = Some(fields);
        Ok(self)
    }
    pub fn endpoint(&self) -> &BedrockEndpoint {
        &self.endpoint
    }
    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }
    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
}
pub struct BedrockAdapter<'a, T> {
    config: BedrockConfig,
    transport: T,
    store: &'a dyn auth::CredentialStore,
}
impl<'a, T: HttpTransport> BedrockAdapter<'a, T> {
    pub fn new(config: BedrockConfig, transport: T, store: &'a dyn auth::CredentialStore) -> Self {
        Self {
            config,
            transport,
            store,
        }
    }
    pub fn config(&self) -> &BedrockConfig {
        &self.config
    }
    pub fn invoke_sync(
        &self,
        request: CanonicalModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, ProviderError> {
        self.invoke_inner(request, cancel, &mut |_| {})
    }
    pub fn invoke_sync_streaming(
        &self,
        request: CanonicalModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn FnMut(&str),
    ) -> Result<ModelStream, ProviderError> {
        self.invoke_inner(request, cancel, sink)
    }
    fn invoke_inner(
        &self,
        request: CanonicalModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn FnMut(&str),
    ) -> Result<ModelStream, ProviderError> {
        cancel.check()?;
        let url = self
            .config
            .endpoint
            .request_url(request.model().model().as_str())?;
        let replay_endpoint = self
            .config
            .endpoint
            .replay_endpoint(request.model().model().as_str())?;
        super::replay::validate(
            &request,
            Some(ReplayProtocol::BedrockConverse),
            &self.config.profile,
            &replay_endpoint,
        )?;
        if request.model().provider() != self.config.profile.provider()
            || (!request.tools().is_empty() && !self.config.capabilities.tools())
        {
            return Err(ProviderError::InvalidRequest);
        }
        let body = encode_bedrock_payload(&request, &self.config, cancel)?;
        let prefix = context_fingerprint(&body["system"], &body["messages"]);
        let bytes = serde_json::to_vec(&body).map_err(|_| ProviderError::InvalidRequest)?;
        if bytes.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        let credential = CredentialResolver::new(self.store).resolve(
            self.config.profile.provider(),
            &self.config.profile,
            cancel,
        )?;
        let headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/vnd.amazon.eventstream".to_owned(),
            ),
            (
                "x-rapidlm-request-id".to_owned(),
                request.request_id().as_str().to_owned(),
            ),
        ];
        let outbound = ProviderHttpRequest::new(&url, &headers, &bytes, &credential)
            .with_auth_style(if self.config.endpoint.signed {
                ProviderAuthStyle::AwsSigV4
            } else {
                ProviderAuthStyle::Bearer
            });
        let mut frames = FrameDecoder::new();
        let mut stream = ConverseStream::default();
        let response = self
            .transport
            .execute_streaming_bytes(&outbound, cancel, &mut |chunk| {
                cancel.check()?;
                frames.feed(chunk, &mut |frame| stream.feed(frame, sink, cancel))
            })?;
        classify_http_error(&response)?;
        frames.finish()?;
        stream.finish(
            &request,
            &self.config.profile,
            &replay_endpoint,
            &prefix,
            cancel,
        )
    }
}
impl<T: HttpTransport> ProviderAdapter for BedrockAdapter<'_, T> {
    fn capabilities(&self) -> ProviderCapabilities {
        self.config.capabilities.clone()
    }
    fn invoke(
        &self,
        request: CanonicalModelRequest,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        let result = self.invoke_sync(request, &cancel);
        async move { result }
    }
}
fn context_fingerprint(system: &Value, messages: &Value) -> String {
    scope_key(&[&json!({"system":system,"messages":messages}).to_string()])
}
fn content(part: &ContentPart, vision: bool) -> Result<Value, ProviderError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"text":text})),
        ContentPart::ImageData { data_url } if vision => {
            let (prefix, data) = data_url
                .split_once(";base64,")
                .ok_or(ProviderError::InvalidRequest)?;
            let format = match prefix {
                "data:image/png" => "png",
                "data:image/jpeg" => "jpeg",
                "data:image/gif" => "gif",
                "data:image/webp" => "webp",
                _ => return Err(ProviderError::InvalidRequest),
            };
            STANDARD
                .decode(data)
                .map_err(|_| ProviderError::InvalidRequest)?;
            Ok(json!({"image":{"format":format,"source":{"bytes":data}}}))
        }
        _ => Err(ProviderError::InvalidRequest),
    }
}
fn append_message(messages: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
        last["content"]
            .as_array_mut()
            .expect("constructed content")
            .extend(parts);
    } else {
        messages.push(json!({"role":role,"content":parts}));
    }
}
pub fn encode_bedrock_payload(
    request: &CanonicalModelRequest,
    config: &BedrockConfig,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    cancel.check()?;
    let max_output = request
        .max_output_tokens()
        .unwrap_or(config.capabilities.max_output());
    if max_output == 0 || max_output > config.capabilities.max_output() {
        return Err(ProviderError::InvalidRequest);
    }
    if let Some(super::anthropic::AnthropicThinking::Enabled { budget_tokens }) = config.thinking
        && budget_tokens >= max_output
    {
        return Err(ProviderError::InvalidRequest);
    }
    let mut system = Vec::new();
    let mut messages = Vec::new();
    let mut pending = BTreeSet::new();
    let vision = config.capabilities.vision();
    for message in request
        .messages()
        .iter()
        .filter(|m| m.role() == MessageRole::System)
    {
        for part in message.parts() {
            match part {
                ContentPart::Text { text } => system.push(json!({"text":text})),
                _ => return Err(ProviderError::InvalidRequest),
            }
        }
    }
    if config.capabilities.caching() && !system.is_empty() {
        system.push(json!({"cachePoint":{"type":"default"}}));
    }
    for message in request
        .messages()
        .iter()
        .filter(|m| m.role() != MessageRole::System)
    {
        cancel.check()?;
        if message.role() == MessageRole::Tool {
            let id = message
                .tool_call_id()
                .ok_or(ProviderError::InvalidRequest)?
                .as_str();
            if !pending.remove(id) {
                return Err(ProviderError::InvalidRequest);
            }
            let mut parts = message
                .parts()
                .iter()
                .map(|p| content(p, vision))
                .collect::<Result<Vec<_>, _>>()?;
            if parts.is_empty() {
                parts.push(json!({"text":""}));
            }
            append_message(
                &mut messages,
                "user",
                vec![json!({"toolResult":{"toolUseId":id,"content":parts}})],
            );
            continue;
        }
        if !pending.is_empty() {
            return Err(ProviderError::InvalidRequest);
        }
        let mut parts = Vec::new();
        if message.role() == MessageRole::Assistant {
            let state = message.provider_replay();
            let compatible_context=state.is_some_and(|s|s.items().iter().any(|i|matches!(&i.data,ReplayData::BedrockContext{sha256} if sha256==&context_fingerprint(&json!(system),&json!(messages)))));
            if state.is_none() {
                for part in message.parts() {
                    parts.push(content(part, false)?);
                }
            }
            for position in 0..=message.tool_calls().len() {
                if let Some(state) = state {
                    for item in state.items().iter().filter(|i| i.before_call == position) {
                        match &item.data {
                            ReplayData::BedrockContext { .. } => {}
                            ReplayData::BedrockText { text } => parts.push(json!({"text":text})),
                            ReplayData::BedrockReasoning {
                                text,
                                signature,
                                redacted_content,
                            } if compatible_context => {
                                if let Some(data) = redacted_content {
                                    STANDARD
                                        .decode(data)
                                        .map_err(|_| ProviderError::InvalidRequest)?;
                                    parts
                                        .push(json!({"reasoningContent":{"redactedContent":data}}));
                                } else {
                                    let mut block = json!({"text":text.as_deref().ok_or(ProviderError::InvalidRequest)?});
                                    if let Some(signature) = signature {
                                        block["signature"] = signature.as_str().into();
                                    }
                                    parts.push(json!({"reasoningContent":{"reasoningText":block}}));
                                }
                            }
                            ReplayData::BedrockReasoning { .. } => {}
                            _ => return Err(ProviderError::InvalidRequest),
                        }
                    }
                }
                if let Some(call) = message.tool_calls().get(position) {
                    if !native_identifier(call.call_id().as_str())
                        || !native_identifier(call.name().as_str())
                    {
                        return Err(ProviderError::InvalidRequest);
                    }
                    if !pending.insert(call.call_id().as_str().to_owned()) {
                        return Err(ProviderError::InvalidRequest);
                    }
                    let input: Value = serde_json::from_str(call.arguments())
                        .map_err(|_| ProviderError::InvalidRequest)?;
                    if !input.is_object() {
                        return Err(ProviderError::InvalidRequest);
                    }
                    parts.push(json!({"toolUse":{"toolUseId":call.call_id().as_str(),"name":call.name().as_str(),"input":input}}));
                }
            }
            append_message(&mut messages, "assistant", parts);
        } else {
            for part in message.parts() {
                parts.push(content(part, vision)?);
            }
            append_message(&mut messages, "user", parts);
        }
    }
    if messages.is_empty() || !pending.is_empty() {
        return Err(ProviderError::InvalidRequest);
    }
    let mut payload = json!({"system":system,"messages":messages});
    payload["inferenceConfig"] = json!({"maxTokens":max_output});
    if request
        .tools()
        .iter()
        .any(|tool| !native_identifier(tool.name().as_str()))
    {
        return Err(ProviderError::InvalidRequest);
    }
    if !request.tools().is_empty() {
        if !config.capabilities.tools() {
            return Err(ProviderError::InvalidRequest);
        }
        payload["toolConfig"] = json!({"tools":request.tools().iter().map(|tool|json!({"toolSpec":{"name":tool.name().as_str(),"description":tool.description(),"inputSchema":{"json":tool.parameters()}}})).collect::<Vec<_>>()});
    }
    if let Some(fields) = &config.additional_fields {
        payload["additionalModelRequestFields"] = fields.clone();
    }
    if let Some(thinking) = config.thinking {
        payload["additionalModelRequestFields"]["thinking"] = match thinking {
            super::anthropic::AnthropicThinking::Adaptive => json!({"type":"adaptive"}),
            super::anthropic::AnthropicThinking::Enabled { budget_tokens } => {
                json!({"type":"enabled","budget_tokens":budget_tokens})
            }
        };
    }
    if let Some(effort) = request.reasoning_effort() {
        if config.capabilities.reasoning() == crate::ReasoningSupport::None {
            return Err(ProviderError::InvalidRequest);
        }
        use crate::ReasoningEffort as E;
        match config.effort_mode {
            BedrockEffortMode::Unspecified => return Err(ProviderError::InvalidRequest),
            BedrockEffortMode::Anthropic => {
                let effort = match effort {
                    E::Low => "low",
                    E::Medium => "medium",
                    E::High => "high",
                    _ => return Err(ProviderError::InvalidRequest),
                };
                payload["additionalModelRequestFields"]["output_config"]["effort"] = effort.into();
                let fields = payload["additionalModelRequestFields"]
                    .as_object_mut()
                    .ok_or(ProviderError::InvalidRequest)?;
                let betas = fields
                    .entry("anthropic_beta")
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .ok_or(ProviderError::InvalidRequest)?;
                if !betas.iter().any(|v| v == "effort-2025-11-24") {
                    betas.push(json!("effort-2025-11-24"));
                }
            }
            BedrockEffortMode::Nova => {
                let mode = match effort {
                    E::None => json!({"type":"disabled"}),
                    E::Low => json!({"type":"enabled","maxReasoningEffort":"low"}),
                    E::Medium => json!({"type":"enabled","maxReasoningEffort":"medium"}),
                    E::High => json!({"type":"enabled","maxReasoningEffort":"high"}),
                    _ => return Err(ProviderError::InvalidRequest),
                };
                payload["additionalModelRequestFields"]["reasoningConfig"] = mode;
            }
        }
    }
    Ok(payload)
}

#[derive(Default)]
struct ConverseStream {
    started: bool,
    stop: Option<String>,
    usage: Option<NormalizedUsage>,
    blocks: BTreeMap<u64, Block>,
}
#[derive(Default)]
struct Block {
    kind: BlockKind,
    started: bool,
    emitted: usize,
    stopped: bool,
}
#[derive(Default)]
enum BlockKind {
    #[default]
    Empty,
    Text(String),
    Tool {
        id: String,
        name: String,
        input: String,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    Redacted(Vec<u8>),
}
impl ConverseStream {
    fn feed(
        &mut self,
        frame: EventFrame,
        sink: &mut dyn FnMut(&str),
        cancel: &CancellationToken,
    ) -> Result<(), ProviderError> {
        cancel.check()?;
        if frame.header(":message-type") != Some("event") {
            return Err(stream_error(
                frame
                    .header(":exception-type")
                    .or(frame.header(":error-code"))
                    .unwrap_or_default(),
            ));
        }
        let kind = frame
            .header(":event-type")
            .ok_or(ProviderError::Permanent)?;
        if kind.ends_with("Exception") {
            return Err(stream_error(kind));
        }
        if frame
            .header(":content-type")
            .is_some_and(|v| v != "application/json")
        {
            return Err(ProviderError::Permanent);
        }
        let value: Value =
            serde_json::from_slice(&frame.payload).map_err(|_| ProviderError::Permanent)?;
        if !value.is_object() {
            return Err(ProviderError::Permanent);
        }
        match kind {
            "messageStart" => {
                if self.started || self.stop.is_some() || value["role"] != "assistant" {
                    return Err(ProviderError::Permanent);
                }
                self.started = true;
            }
            "contentBlockStart" | "contentBlockDelta" | "contentBlockStop" => {
                if !self.started || self.stop.is_some() {
                    return Err(ProviderError::Permanent);
                }
                let index = value["contentBlockIndex"]
                    .as_u64()
                    .ok_or(ProviderError::Permanent)?;
                if !self.blocks.contains_key(&index) && self.blocks.len() >= MAX_REPLAY_ITEMS {
                    return Err(ProviderError::Permanent);
                }
                if !self.blocks.contains_key(&index)
                    && self
                        .blocks
                        .keys()
                        .next_back()
                        .is_some_and(|last| index < *last)
                {
                    return Err(ProviderError::Permanent);
                }
                let block = self.blocks.entry(index).or_default();
                if block.stopped {
                    return Err(ProviderError::Permanent);
                }
                match kind {
                    "contentBlockStart" => {
                        if block.started || !matches!(block.kind, BlockKind::Empty) {
                            return Err(ProviderError::Permanent);
                        }
                        block.started = true;
                        let start = value["start"].as_object().ok_or(ProviderError::Permanent)?;
                        if start.is_empty() {
                        } else if let Some(tool) = start.get("toolUse").filter(|_| start.len() == 1)
                        {
                            let id = string(tool, "toolUseId")?;
                            let name = string(tool, "name")?;
                            if !native_identifier(id) || !native_identifier(name) {
                                return Err(ProviderError::Permanent);
                            }
                            ToolCallId::parse(id).map_err(|_| ProviderError::Permanent)?;
                            ToolName::parse(name).map_err(|_| ProviderError::Permanent)?;
                            block.kind = BlockKind::Tool {
                                id: id.to_owned(),
                                name: name.to_owned(),
                                input: String::new(),
                            };
                        } else {
                            return Err(ProviderError::Permanent);
                        }
                    }
                    "contentBlockDelta" => {
                        let delta = value["delta"]
                            .as_object()
                            .filter(|m| m.len() == 1)
                            .ok_or(ProviderError::Permanent)?;
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if matches!(block.kind, BlockKind::Empty) {
                                block.kind = BlockKind::Text(String::new());
                            }
                            let BlockKind::Text(full) = &mut block.kind else {
                                return Err(ProviderError::Permanent);
                            };
                            bounded_append(full, text)?;
                        } else if let Some(tool) = delta.get("toolUse") {
                            let BlockKind::Tool { input, .. } = &mut block.kind else {
                                return Err(ProviderError::Permanent);
                            };
                            bounded_append(input, string(tool, "input")?)?;
                        } else if let Some(reasoning) = delta
                            .get("reasoningContent")
                            .and_then(Value::as_object)
                            .filter(|m| m.len() == 1)
                        {
                            if let Some(redacted) =
                                reasoning.get("redactedContent").and_then(Value::as_str)
                            {
                                if matches!(block.kind, BlockKind::Empty) {
                                    block.kind = BlockKind::Redacted(Vec::new());
                                }
                                let BlockKind::Redacted(full) = &mut block.kind else {
                                    return Err(ProviderError::Permanent);
                                };
                                let bytes = STANDARD
                                    .decode(redacted)
                                    .map_err(|_| ProviderError::Permanent)?;
                                if full
                                    .len()
                                    .checked_add(bytes.len())
                                    .is_none_or(|n| n > MAX_REPLAY_BYTES)
                                {
                                    return Err(ProviderError::Permanent);
                                }
                                full.extend(bytes);
                            } else {
                                if matches!(block.kind, BlockKind::Empty) {
                                    block.kind = BlockKind::Reasoning {
                                        text: String::new(),
                                        signature: None,
                                    };
                                }
                                let BlockKind::Reasoning { text, signature } = &mut block.kind
                                else {
                                    return Err(ProviderError::Permanent);
                                };
                                if let Some(fragment) =
                                    reasoning.get("text").and_then(Value::as_str)
                                {
                                    bounded_append(text, fragment)?;
                                } else if let Some(fragment) =
                                    reasoning.get("signature").and_then(Value::as_str)
                                {
                                    bounded_append(signature.get_or_insert_default(), fragment)?;
                                } else {
                                    return Err(ProviderError::Permanent);
                                }
                            }
                        } else {
                            return Err(ProviderError::Permanent);
                        }
                    }
                    "contentBlockStop" => {
                        if matches!(block.kind, BlockKind::Empty) {
                            return Err(ProviderError::Permanent);
                        }
                        if let BlockKind::Tool { input, .. } = &mut block.kind {
                            let value: Value = serde_json::from_str(input)
                                .map_err(|_| ProviderError::Permanent)?;
                            if !value.is_object() {
                                return Err(ProviderError::Permanent);
                            }
                            *input = value.to_string();
                        }
                        block.stopped = true;
                    }
                    _ => unreachable!(),
                }
            }
            "messageStop" => {
                if !self.started || self.stop.is_some() || self.blocks.values().any(|b| !b.stopped)
                {
                    return Err(ProviderError::Permanent);
                }
                self.stop = Some(string(&value, "stopReason")?.to_owned());
            }
            "metadata" => {
                if self.stop.is_none() || self.usage.is_some() {
                    return Err(ProviderError::Permanent);
                }
                self.usage = Some(parse_usage(&value["usage"])?);
            }
            _ => return Err(ProviderError::Permanent),
        }
        // A later text block waits until earlier text blocks are complete.
        // Tools/reasoning never render, and cannot later become text.
        for block in self.blocks.values_mut() {
            match &block.kind {
                BlockKind::Text(text) => {
                    if block.emitted < text.len() {
                        sink(&text[block.emitted..]);
                        block.emitted = text.len();
                    }
                    if !block.stopped {
                        break;
                    }
                }
                BlockKind::Empty if !block.stopped => break,
                _ => {}
            }
        }
        Ok(())
    }
    fn finish(
        self,
        request: &CanonicalModelRequest,
        profile: &ProviderProfile,
        url: &str,
        prefix: &str,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, ProviderError> {
        let mut events = Vec::new();
        let mut items = vec![ReplayItem {
            before_call: 0,
            data: ReplayData::BedrockContext {
                sha256: prefix.to_owned(),
            },
        }];
        let mut calls = 0;
        let mut ids = BTreeSet::new();
        for block in self.blocks.into_values() {
            if !block.stopped {
                return Err(ProviderError::Permanent);
            }
            match block.kind {
                BlockKind::Text(text) => {
                    push_text_deltas(&mut events, &text)?;
                    items.push(ReplayItem {
                        before_call: calls,
                        data: ReplayData::BedrockText { text },
                    });
                }
                BlockKind::Tool { id, name, input } => {
                    if !ids.insert(id.clone()) {
                        return Err(ProviderError::Permanent);
                    }
                    let id = ToolCallId::parse(&id).map_err(|_| ProviderError::Permanent)?;
                    push_event(
                        &mut events,
                        ModelStreamEvent::ToolCallStart {
                            call_id: id.clone(),
                            name: ToolName::parse(&name).map_err(|_| ProviderError::Permanent)?,
                        },
                    )?;
                    push_argument_deltas(&mut events, id, &input)?;
                    calls += 1;
                }
                BlockKind::Reasoning { text, signature } => items.push(ReplayItem {
                    before_call: calls,
                    data: ReplayData::BedrockReasoning {
                        text: Some(text),
                        signature,
                        redacted_content: None,
                    },
                }),
                BlockKind::Redacted(data) => items.push(ReplayItem {
                    before_call: calls,
                    data: ReplayData::BedrockReasoning {
                        text: None,
                        signature: None,
                        redacted_content: Some(STANDARD.encode(data)),
                    },
                }),
                BlockKind::Empty => return Err(ProviderError::Permanent),
            }
        }
        let reason = match self.stop.as_deref() {
            Some("end_turn" | "stop_sequence") if calls == 0 => FinishReason::Stop,
            Some("max_tokens") if calls == 0 => FinishReason::Length,
            Some("tool_use") if calls > 0 => FinishReason::ToolCalls,
            _ => return Err(ProviderError::Permanent),
        };
        let usage = self.usage.unwrap_or_else(|| {
            NormalizedUsage::new(None, None, None, None, None, None, UsageCost::Unknown)
        });
        push_event(
            &mut events,
            ModelStreamEvent::Completed {
                finish: reason,
                usage,
            },
        )?;
        let stream = ModelStream::from_events(
            request.request_id().clone(),
            request.model().clone(),
            events,
            cancel,
        )?;
        if calls == 0 {
            return Ok(stream);
        }
        let state = ProviderReplayState::new(
            ReplayProtocol::BedrockConverse,
            profile.provider().as_str(),
            request.model().model().as_str(),
            &super::replay::scope(profile, url),
            super::replay::bindings(&stream)?,
            items,
        )
        .map_err(|_| ProviderError::Permanent)?;
        stream
            .with_provider_replay(Some(state))
            .map_err(|_| ProviderError::Permanent)
    }
}
fn bounded_append(target: &mut String, part: &str) -> Result<(), ProviderError> {
    if target
        .len()
        .checked_add(part.len())
        .is_none_or(|n| n > MAX_REPLAY_BYTES)
        || part.contains('\0')
    {
        return Err(ProviderError::Permanent);
    }
    target.push_str(part);
    Ok(())
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(ProviderError::Permanent)
}
fn stream_error(kind: &str) -> ProviderError {
    match kind {
        "throttlingException" => ProviderError::RateLimited {
            retry_after_ms: None,
        },
        "internalServerException"
        | "modelStreamErrorException"
        | "modelTimeoutException"
        | "serviceUnavailableException" => ProviderError::Transient,
        "accessDeniedException" | "unrecognizedClientException" => ProviderError::AuthFailed,
        _ => ProviderError::Permanent,
    }
}
fn parse_usage(value: &Value) -> Result<NormalizedUsage, ProviderError> {
    let map = value.as_object().ok_or(ProviderError::Permanent)?;
    let get = |key: &str| -> Result<Option<u64>, ProviderError> {
        map.get(key)
            .filter(|v| !v.is_null())
            .map(|v| v.as_u64().ok_or(ProviderError::Permanent))
            .transpose()
    };
    let input = get("inputTokens")?;
    let output = get("outputTokens")?;
    let cached = get("cacheReadInputTokens")?;
    let write = get("cacheWriteInputTokens")?;
    // Bedrock reports ordinary input separately from cache reads/writes.
    let total_input = if input.is_none() && cached.is_none() && write.is_none() {
        None
    } else {
        Some(
            input
                .unwrap_or(0)
                .checked_add(cached.unwrap_or(0))
                .and_then(|n| n.checked_add(write.unwrap_or(0)))
                .ok_or(ProviderError::Permanent)?,
        )
    };
    let uncached = if input.is_none() && write.is_none() {
        None
    } else {
        Some(
            input
                .unwrap_or(0)
                .checked_add(write.unwrap_or(0))
                .ok_or(ProviderError::Permanent)?,
        )
    };
    let mut usage = NormalizedUsage::new(
        total_input,
        cached,
        uncached,
        output,
        None,
        None,
        UsageCost::Unknown,
    );
    for (key, val) in [
        ("cacheWriteInputTokens", write),
        ("totalTokens", get("totalTokens")?),
    ] {
        if let Some(value) = val {
            usage.insert_extra(key, UsageExtValue::U64(value))?;
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::super::aws_eventstream::{HeaderValue, encode_frame};
    use super::*;
    use crate::{
        CanonicalMessage, CanonicalToolSpec, CatalogRevision, Http1Transport, ModelId,
        ModelPurpose, ModelRef, ModelRequestId, ProviderId, ReasoningSupport, StaticWireAuth,
        ToolCall, UsageFieldSet,
    };
    use protocol::{Baggage, TraceContext, TraceId};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Duration,
    };
    fn profile() -> ProviderProfile {
        ProviderProfile::new(
            "work",
            ProviderId::parse("bedrock").unwrap(),
            auth::SecretRef::from_alias("env:AWS_BEARER_TOKEN_BEDROCK").unwrap(),
        )
        .unwrap()
    }
    fn config(base: &str) -> BedrockConfig {
        BedrockConfig::new(
            profile(),
            BedrockEndpoint::new(base, "us-east-1", false).unwrap(),
            ProviderCapabilities::new(
                true,
                true,
                true,
                true,
                ReasoningSupport::Exposed,
                true,
                32768,
                4096,
                UsageFieldSet::new(true, true, true, true, false, false, false),
            )
            .unwrap(),
        )
        .unwrap()
    }
    fn request() -> CanonicalModelRequest {
        CanonicalModelRequest::new(ModelRequestId::parse("req-bedrock-1").unwrap(),ModelRef::new(profile().provider().clone(),ModelId::parse("anthropic.fixture-v1:0").unwrap()),ModelPurpose::Code,
        vec![CanonicalMessage::new(MessageRole::System,vec![ContentPart::text("system instructions").unwrap()],None,vec![]).unwrap(),CanonicalMessage::new(MessageRole::User,vec![ContentPart::text("task").unwrap()],None,vec![]).unwrap()],
        vec![CanonicalToolSpec::new(ToolName::parse("read").unwrap(),"read a file",json!({"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false})).unwrap()],Some(512),CatalogRevision::new(1).unwrap(),TraceContext::new(TraceId::new(),None,Baggage::empty()),&CancellationToken::new()).unwrap()
    }
    #[test]
    fn bedrock_effort_is_explicit_native_and_cannot_override_output_caps() {
        use crate::ReasoningEffort as E;
        let cfg = config("https://owned.example");
        for mode in [
            BedrockEffortMode::Unspecified,
            BedrockEffortMode::Anthropic,
            BedrockEffortMode::Nova,
        ] {
            for effort in [
                E::None,
                E::Minimal,
                E::Low,
                E::Medium,
                E::High,
                E::XHigh,
                E::Ultra,
            ] {
                let config = cfg.clone().with_effort_mode(mode);
                let request = request().with_reasoning_effort(effort);
                let payload = encode_bedrock_payload(&request, &config, &CancellationToken::new());
                let supported = mode != BedrockEffortMode::Unspecified
                    && matches!(effort, E::Low | E::Medium | E::High)
                    || mode == BedrockEffortMode::Nova && effort == E::None;
                assert_eq!(payload.is_ok(), supported, "{mode:?}/{effort:?}");
                if let Ok(payload) = payload {
                    assert_eq!(payload["inferenceConfig"]["maxTokens"], 512);
                    let fields = &payload["additionalModelRequestFields"];
                    if mode == BedrockEffortMode::Anthropic {
                        assert_eq!(fields["output_config"]["effort"], effort.name());
                        assert_eq!(fields["anthropic_beta"], json!(["effort-2025-11-24"]));
                    } else if effort == E::None {
                        assert_eq!(fields["reasoningConfig"]["type"], "disabled");
                    } else {
                        assert_eq!(
                            fields["reasoningConfig"]["maxReasoningEffort"],
                            effort.name()
                        );
                    }
                }
            }
        }
        for fields in [
            json!({"maxTokens":9999}),
            json!({"max_tokens":9999}),
            json!({"inferenceConfig":{"maxTokens":9999}}),
            json!({"output_config":42}),
        ] {
            assert!(cfg.clone().with_additional_fields(fields).is_err());
        }
        let thinking = cfg
            .clone()
            .with_effort_mode(BedrockEffortMode::Anthropic)
            .with_thinking(super::super::anthropic::AnthropicThinking::Enabled {
                budget_tokens: 1024,
            })
            .unwrap();
        assert!(encode_bedrock_payload(&request(), &thinking, &CancellationToken::new()).is_err());
        assert!(
            cfg.clone()
                .with_thinking(super::super::anthropic::AnthropicThinking::Adaptive)
                .is_err()
        );
        let invalid_beta = cfg
            .with_effort_mode(BedrockEffortMode::Anthropic)
            .with_additional_fields(json!({"anthropic_beta":"wrong type"}))
            .unwrap();
        assert!(
            encode_bedrock_payload(
                &request().with_reasoning_effort(E::High),
                &invalid_beta,
                &CancellationToken::new()
            )
            .is_err()
        );
    }
    fn event(kind: &str, payload: Value) -> EventFrame {
        EventFrame {
            headers: BTreeMap::from([
                (
                    ":message-type".to_owned(),
                    HeaderValue::String("event".to_owned()),
                ),
                (
                    ":event-type".to_owned(),
                    HeaderValue::String(kind.to_owned()),
                ),
            ]),
            payload: serde_json::to_vec(&payload).unwrap(),
        }
    }
    fn feed(stream: &mut ConverseStream, kind: &str, value: Value) -> Result<(), ProviderError> {
        stream.feed(event(kind, value), &mut |_| {}, &CancellationToken::new())
    }
    fn tool_stream(req: &CanonicalModelRequest, cfg: &BedrockConfig) -> ModelStream {
        let mut stream = ConverseStream::default();
        let mut visible = String::new();
        let frames = vec![
            ("messageStart", json!({"role":"assistant"})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"private-bedrock-thought"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"signed-bedrock-"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"continuation"}}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":0})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":1,"delta":{"text":"visible preface"}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":1})),
            (
                "contentBlockStart",
                json!({"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"read1","name":"read"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":2,"delta":{"toolUse":{"input":"{ \"path\" : "}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":2,"delta":{"toolUse":{"input":"\"a.txt\" }"}}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":2})),
            ("messageStop", json!({"stopReason":"tool_use"})),
            (
                "metadata",
                json!({"usage":{"inputTokens":3,"cacheReadInputTokens":5,"cacheWriteInputTokens":2,"outputTokens":4,"totalTokens":14}}),
            ),
        ];
        for (kind, value) in frames {
            stream
                .feed(
                    event(kind, value),
                    &mut |text| visible.push_str(text),
                    &CancellationToken::new(),
                )
                .unwrap();
        }
        assert_eq!(visible, "visible preface");
        let payload = encode_bedrock_payload(req, cfg, &CancellationToken::new()).unwrap();
        stream
            .finish(
                req,
                &profile(),
                &cfg.endpoint
                    .replay_endpoint(req.model().model().as_str())
                    .unwrap(),
                &context_fingerprint(&payload["system"], &payload["messages"]),
                &CancellationToken::new(),
            )
            .unwrap()
    }
    fn continuation(req: &CanonicalModelRequest, stream: &ModelStream) -> CanonicalModelRequest {
        let calls = stream
            .events()
            .iter()
            .filter_map(|event| {
                if let ModelStreamEvent::ToolCallStart { call_id, name } = event {
                    let args = stream
                        .events()
                        .iter()
                        .filter_map(|e| {
                            if let ModelStreamEvent::ToolCallArgumentsDelta {
                                call_id: id,
                                arguments_delta,
                            } = e
                            {
                                (id == call_id).then_some(arguments_delta.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<String>();
                    Some(ToolCall::new(call_id.clone(), name.clone(), args).unwrap())
                } else {
                    None
                }
            })
            .collect();
        let encoded = serde_json::to_vec(stream.provider_replay().unwrap()).unwrap();
        let restored: ProviderReplayState = serde_json::from_slice(&encoded).unwrap();
        assert!(!format!("{restored:?}").contains("private-bedrock"));
        let assistant = CanonicalMessage::new(
            MessageRole::Assistant,
            vec![ContentPart::text("must not duplicate a synthetic preface").unwrap()],
            None,
            calls,
        )
        .unwrap()
        .with_provider_replay(restored)
        .unwrap();
        let tool = CanonicalMessage::new(
            MessageRole::Tool,
            vec![
                ContentPart::text("file result").unwrap(),
                ContentPart::image_data("data:image/png;base64,aGVsbG8=").unwrap(),
            ],
            Some(ToolCallId::parse("read1").unwrap()),
            vec![],
        )
        .unwrap();
        let mut messages = req.messages().to_vec();
        messages.extend([assistant, tool]);
        req.clone().with_messages(messages).unwrap()
    }
    #[test]
    fn bedrock_native_payload_preserves_images_tools_bounds_and_explicit_resource_routing() {
        let cfg = config("http://127.0.0.1:8080");
        let mut req = request();
        let mut messages = req.messages().to_vec();
        messages.push(
            CanonicalMessage::new(
                MessageRole::User,
                vec![ContentPart::image_data("data:image/jpeg;base64,aGVsbG8=").unwrap()],
                None,
                vec![],
            )
            .unwrap(),
        );
        req = req.with_messages(messages).unwrap();
        let payload = encode_bedrock_payload(&req, &cfg, &CancellationToken::new()).unwrap();
        assert_eq!(payload["system"][1]["cachePoint"]["type"], "default");
        assert_eq!(
            payload["messages"][0]["content"][1]["image"]["source"]["bytes"],
            "aGVsbG8="
        );
        assert_eq!(payload["inferenceConfig"]["maxTokens"], 512);
        assert_eq!(
            payload["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["additionalProperties"],
            false
        );
        assert!(payload.get("model").is_none());
        assert!(
            cfg.endpoint
                .request_url(req.model().model().as_str())
                .unwrap()
                .ends_with("/model/anthropic.fixture-v1%3A0/converse-stream")
        );
        let endpoint = cfg
            .endpoint
            .clone()
            .with_resource("arn:aws:bedrock:us-east-1:123456789012:inference-profile/test")
            .unwrap();
        assert!(
            endpoint.request_url("friendly-name").unwrap().contains(
                "arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Ftest"
            )
        );
        for resource in ["../bad", "model?secret=leak", "model#fragment", "bad\\path"] {
            assert!(cfg.endpoint.clone().with_resource(resource).is_err());
        }
        let additional = cfg
            .clone()
            .with_additional_fields(json!({"thinking":{"type":"enabled","budget_tokens":1024}}))
            .unwrap();
        assert_eq!(
            encode_bedrock_payload(&req, &additional, &CancellationToken::new()).unwrap()["additionalModelRequestFields"]
                ["thinking"]["budget_tokens"],
            1024
        );
    }
    #[test]
    fn bedrock_signed_thoughts_and_visible_preludes_replay_after_serialization_with_native_prefix_binding()
     {
        let cfg = config("https://bedrock-runtime.us-east-1.amazonaws.com");
        let req = request();
        let stream = tool_stream(&req, &cfg);
        let state = stream.provider_replay().unwrap();
        assert!(super::super::replay::matches(
            state,
            ReplayProtocol::BedrockConverse,
            &profile(),
            &cfg.endpoint
                .replay_endpoint(req.model().model().as_str())
                .unwrap(),
            req.model().model().as_str()
        ));
        let other = BedrockEndpoint::new(cfg.endpoint.base_url(), "eu-west-1", false).unwrap();
        assert!(!super::super::replay::matches(
            state,
            ReplayProtocol::BedrockConverse,
            &profile(),
            &other.replay_endpoint(req.model().model().as_str()).unwrap(),
            req.model().model().as_str()
        ));
        let next = continuation(&req, &stream);
        let payload = encode_bedrock_payload(&next, &cfg, &CancellationToken::new()).unwrap();
        let assistant = &payload["messages"][1]["content"];
        assert_eq!(
            assistant[0]["reasoningContent"]["reasoningText"]["signature"],
            "signed-bedrock-continuation"
        );
        assert_eq!(assistant[1]["text"], "visible preface");
        assert_eq!(assistant[2]["toolUse"]["input"]["path"], "a.txt");
        assert_eq!(
            payload["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "read1"
        );
        assert_eq!(
            payload["messages"][2]["content"][0]["toolResult"]["content"][1]["image"]["source"]["bytes"],
            "aGVsbG8="
        );
        assert!(!payload.to_string().contains("must not duplicate"));
        let mut messages = next.messages().to_vec();
        messages[0] = CanonicalMessage::new(
            MessageRole::System,
            vec![ContentPart::text("changed system").unwrap()],
            None,
            vec![],
        )
        .unwrap();
        let changed = encode_bedrock_payload(
            &next.with_messages(messages).unwrap(),
            &cfg,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(!changed.to_string().contains("signed-bedrock-continuation"));
        assert!(changed.to_string().contains("visible preface"));
        let usage = stream
            .events()
            .iter()
            .find_map(|e| {
                if let ModelStreamEvent::Completed { usage, .. } = e {
                    Some(usage)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(usage.input_tokens(), Some(10));
        assert_eq!(usage.cached_input_tokens(), Some(5));
        assert_eq!(usage.uncached_input_tokens(), Some(5));
        assert_eq!(usage.output_tokens(), Some(4));
    }
    #[test]
    fn bedrock_rejects_incomplete_tools_duplicate_ids_malformed_unions_and_unfinished_messages() {
        let cfg = config("http://127.0.0.1:8080");
        let req = request();
        let prefix = "0".repeat(64);
        let mut stream = ConverseStream::default();
        feed(&mut stream, "messageStart", json!({"role":"assistant"})).unwrap();
        feed(
            &mut stream,
            "contentBlockStart",
            json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"read1","name":"read"}}}),
        )
        .unwrap();
        feed(
            &mut stream,
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{"}}}),
        )
        .unwrap();
        assert!(
            feed(
                &mut stream,
                "contentBlockStop",
                json!({"contentBlockIndex":0})
            )
            .is_err()
        );
        assert!(
            stream
                .finish(
                    &req,
                    &profile(),
                    &cfg.endpoint.request_url("x").unwrap(),
                    &prefix,
                    &CancellationToken::new()
                )
                .is_err()
        );
        let mut stream = ConverseStream::default();
        feed(&mut stream, "messageStart", json!({"role":"assistant"})).unwrap();
        assert!(feed(&mut stream,"contentBlockDelta",json!({"contentBlockIndex":0,"delta":{"text":"visible","reasoningContent":{"text":"private"}}})).is_err());
        for name in ["bad.tool", &"x".repeat(65)] {
            let mut stream = ConverseStream::default();
            feed(&mut stream, "messageStart", json!({"role":"assistant"})).unwrap();
            assert!(feed(&mut stream,"contentBlockStart",json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"id","name":name}}})).is_err());
        }
        let mut stream = ConverseStream::default();
        feed(&mut stream, "messageStart", json!({"role":"assistant"})).unwrap();
        for index in 0..2 {
            feed(&mut stream,"contentBlockStart",json!({"contentBlockIndex":index,"start":{"toolUse":{"toolUseId":"same","name":"read"}}})).unwrap();
            feed(
                &mut stream,
                "contentBlockDelta",
                json!({"contentBlockIndex":index,"delta":{"toolUse":{"input":"{}"}}}),
            )
            .unwrap();
            feed(
                &mut stream,
                "contentBlockStop",
                json!({"contentBlockIndex":index}),
            )
            .unwrap();
        }
        feed(&mut stream, "messageStop", json!({"stopReason":"tool_use"})).unwrap();
        assert!(
            stream
                .finish(
                    &req,
                    &profile(),
                    "http://localhost",
                    &prefix,
                    &CancellationToken::new()
                )
                .is_err()
        );
    }
    #[test]
    fn bedrock_orders_interleaved_text_and_keeps_redacted_bytes_private() {
        let mut stream = ConverseStream::default();
        let mut text = String::new();
        for (kind, value) in [
            ("messageStart", json!({"role":"assistant"})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"text":"first "}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":1,"delta":{"text":"last"}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"text":"middle "}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":0})),
            ("contentBlockStop", json!({"contentBlockIndex":1})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":2,"delta":{"reasoningContent":{"redactedContent":"YQ=="}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":2,"delta":{"reasoningContent":{"redactedContent":"Yg=="}}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":2})),
            ("messageStop", json!({"stopReason":"end_turn"})),
        ] {
            stream
                .feed(
                    event(kind, value),
                    &mut |fragment| text.push_str(fragment),
                    &CancellationToken::new(),
                )
                .unwrap();
        }
        assert_eq!(text, "first middle last");
        assert!(matches!(&stream.blocks[&2].kind,BlockKind::Redacted(bytes) if bytes==b"ab"));
        let result = stream
            .finish(
                &request(),
                &profile(),
                "http://localhost",
                &"0".repeat(64),
                &CancellationToken::new(),
            )
            .unwrap();
        let visible = result
            .events()
            .iter()
            .filter_map(|e| {
                if let ModelStreamEvent::TextDelta { text } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<String>();
        assert_eq!(visible, text);
    }
    #[test]
    fn bedrock_many_small_wire_deltas_coalesce_under_the_canonical_event_bound() {
        let mut stream = ConverseStream::default();
        feed(&mut stream, "messageStart", json!({"role":"assistant"})).unwrap();
        for _ in 0..5000 {
            feed(
                &mut stream,
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"text":"x"}}),
            )
            .unwrap();
        }
        feed(
            &mut stream,
            "contentBlockStop",
            json!({"contentBlockIndex":0}),
        )
        .unwrap();
        feed(&mut stream, "messageStop", json!({"stopReason":"end_turn"})).unwrap();
        let result = stream
            .finish(
                &request(),
                &profile(),
                "http://localhost",
                &"0".repeat(64),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(result.events().len() < 5);
        assert_eq!(
            result
                .events()
                .iter()
                .filter_map(|e| if let ModelStreamEvent::TextDelta { text } = e {
                    Some(text.len())
                } else {
                    None
                })
                .sum::<usize>(),
            5000
        );
    }
    #[test]
    fn bedrock_exception_frames_are_typed_and_never_echo_provider_error_content() {
        for (kind, expected) in [
            (
                "throttlingException",
                ProviderError::RateLimited {
                    retry_after_ms: None,
                },
            ),
            ("internalServerException", ProviderError::Transient),
            ("accessDeniedException", ProviderError::AuthFailed),
            ("validationException", ProviderError::Permanent),
        ] {
            let frame = EventFrame {
                headers: BTreeMap::from([
                    (
                        ":message-type".to_owned(),
                        HeaderValue::String("exception".to_owned()),
                    ),
                    (
                        ":exception-type".to_owned(),
                        HeaderValue::String(kind.to_owned()),
                    ),
                ]),
                payload: b"private-provider-error".to_vec(),
            };
            let error = ConverseStream::default()
                .feed(
                    frame,
                    &mut |_| panic!("no text from errors"),
                    &CancellationToken::new(),
                )
                .unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?}").contains("private-provider-error"));
        }
    }
    #[test]
    fn native_bedrock_adapter_streams_framed_utf8_before_http_completion_without_rendering_thoughts()
     {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen, delivered) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut raw = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap();
                    if body.len() >= length {
                        break;
                    }
                }
            }
            let raw = String::from_utf8(raw).unwrap();
            assert!(raw.starts_with("POST /model/anthropic.fixture-v1%3A0/converse-stream "));
            assert!(raw.contains("Authorization: Bearer bedrock-fixture-token"));
            assert!(raw.contains("\"toolConfig\""));
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/vnd.amazon.eventstream\r\nConnection: close\r\n\r\n").unwrap();
            let mut send = |kind: &str, payload: Value| {
                let frame = encode_frame(
                    &[(":message-type", "event"), (":event-type", kind)],
                    payload.to_string().as_bytes(),
                );
                for bytes in frame.chunks(9) {
                    write!(socket, "{:x}\r\n", bytes.len()).unwrap();
                    socket.write_all(bytes).unwrap();
                    socket.write_all(b"\r\n").unwrap();
                    socket.flush().unwrap();
                }
            };
            send("messageStart", json!({"role":"assistant"}));
            send(
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"private-stream-thought"}}}),
            );
            send("contentBlockStop", json!({"contentBlockIndex":0}));
            send(
                "contentBlockDelta",
                json!({"contentBlockIndex":1,"delta":{"text":"🐈"}}),
            );
            delivered
                .recv_timeout(Duration::from_secs(3))
                .expect("visible native fragment before response completion");
            send("contentBlockStop", json!({"contentBlockIndex":1}));
            send("messageStop", json!({"stopReason":"end_turn"}));
            send(
                "metadata",
                json!({"usage":{"inputTokens":2,"outputTokens":1,"totalTokens":3}}),
            );
            socket.write_all(b"0\r\n\r\n").unwrap();
        });
        use auth::CredentialStore;
        let store = auth::InMemoryCredentialStore::new();
        store
            .put(
                auth::CredentialPut::ephemeral(
                    auth::SecretValue::from_str(
                        profile().credential().clone(),
                        "authority-placeholder",
                    )
                    .unwrap(),
                    auth::CredentialKind::ProviderApiKey,
                )
                .with_provider("bedrock")
                .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let adapter = BedrockAdapter::new(
            config(&format!("http://{addr}")),
            Box::new(Http1Transport::new(
                StaticWireAuth::bearer("bedrock-fixture-token").unwrap(),
            )) as Box<dyn HttpTransport>,
            &store,
        );
        let mut text = String::new();
        let result = adapter
            .invoke_sync_streaming(request(), &CancellationToken::new(), &mut |fragment| {
                text.push_str(fragment);
                seen.send(()).unwrap();
            })
            .unwrap();
        worker.join().unwrap();
        assert_eq!(text, "🐈");
        assert!(matches!(
            result.events().last(),
            Some(ModelStreamEvent::Completed {
                finish: FinishReason::Stop,
                ..
            })
        ));
    }
}
