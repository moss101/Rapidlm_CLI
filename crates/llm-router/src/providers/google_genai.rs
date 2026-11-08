//! Native Google generateContent protocol over the shared HTTP transport.
//! Private signed parts remain bound to their original calls and origin.
use super::openai_compatible::{
    HttpTransport, MAX_HTTP_REQUEST_BYTES, MAX_HTTP_RESPONSE_BYTES, OpenAiApiStyle,
    OpenAiCompatibleEndpoint, ProviderAuthStyle, ProviderHttpRequest, classify_http_error,
    push_argument_deltas, push_event, push_text_deltas, sse_data_blocks,
};
use crate::{
    CancellationToken, CanonicalMessage, CanonicalModelRequest, ContentPart, CredentialResolver,
    FinishReason, MessageRole, ModelStream, ModelStreamEvent, NormalizedUsage, ProviderAdapter,
    ProviderCapabilities, ProviderError, ProviderProfile, ReasoningEffort, ReasoningSupport,
    ToolCallId, ToolName, UsageCost, UsageExtValue,
};
use protocol::provider_replay::{
    MAX_REPLAY_BYTES, MAX_REPLAY_ITEMS, ProviderReplayState, ReplayCall, ReplayData, ReplayItem,
    ReplayProtocol, scope_key,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleGenAiEndpoint {
    base_url: String,
    vertex: Option<super::vertex::VertexRoute>,
}
impl GoogleGenAiEndpoint {
    pub fn new(base_url: &str) -> Result<Self, ProviderError> {
        let checked = OpenAiCompatibleEndpoint::new(base_url, OpenAiApiStyle::ChatCompletions)?;
        Ok(Self {
            base_url: checked.base_url().to_owned(),
            vertex: None,
        })
    }
    pub fn vertex(
        base_url: &str,
        route: super::vertex::VertexRoute,
    ) -> Result<Self, ProviderError> {
        let mut endpoint = Self::new(base_url)?;
        endpoint.vertex = Some(route);
        Ok(endpoint)
    }
    pub fn is_vertex(&self) -> bool {
        self.vertex.is_some()
    }
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    pub fn request_url(&self, model: &str) -> Result<String, ProviderError> {
        if let Some(route) = &self.vertex {
            return route.request_url(
                &self.base_url,
                super::vertex::VertexPublisher::Google,
                model,
            );
        }
        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || model.len() > 256
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(format!(
            "{}/models/{model}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/')
        ))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleGenAiConfig {
    profile: ProviderProfile,
    endpoint: GoogleGenAiEndpoint,
    capabilities: ProviderCapabilities,
}
impl GoogleGenAiConfig {
    pub fn new(
        profile: ProviderProfile,
        endpoint: GoogleGenAiEndpoint,
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
    pub fn endpoint(&self) -> &GoogleGenAiEndpoint {
        &self.endpoint
    }
    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
}
pub struct GoogleGenAiAdapter<'a, T> {
    config: GoogleGenAiConfig,
    transport: T,
    store: &'a dyn auth::CredentialStore,
}
impl<'a, T: HttpTransport> GoogleGenAiAdapter<'a, T> {
    pub fn new(
        config: GoogleGenAiConfig,
        transport: T,
        store: &'a dyn auth::CredentialStore,
    ) -> Self {
        Self {
            config,
            transport,
            store,
        }
    }
    pub fn config(&self) -> &GoogleGenAiConfig {
        &self.config
    }
    pub fn invoke_sync(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, ProviderError> {
        self.invoke_inner(req, cancel, None)
    }
    pub fn invoke_sync_streaming(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn FnMut(&str),
    ) -> Result<ModelStream, ProviderError> {
        self.invoke_inner(req, cancel, Some(sink))
    }
    fn invoke_inner(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
        sink: Option<&mut dyn FnMut(&str)>,
    ) -> Result<ModelStream, ProviderError> {
        cancel.check()?;
        let url = self
            .config
            .endpoint
            .request_url(req.model().model().as_str())?;
        super::replay::validate(
            &req,
            Some(ReplayProtocol::GoogleGenAi),
            &self.config.profile,
            &url,
        )?;
        if req.model().provider() != self.config.profile.provider()
            || (!req.tools().is_empty() && !self.config.capabilities.tools())
        {
            return Err(ProviderError::InvalidRequest);
        }
        if req
            .messages()
            .iter()
            .flat_map(|m| m.parts())
            .any(|p| !matches!(p, ContentPart::Text { .. }))
            && !self.config.capabilities.vision()
        {
            return Err(ProviderError::InvalidRequest);
        }
        if req.reasoning_effort().is_some()
            && self.config.capabilities.reasoning() == ReasoningSupport::None
        {
            return Err(ProviderError::InvalidRequest);
        }
        let payload = encode_google_payload(&req, cancel)?;
        let bytes = serde_json::to_vec(&payload).map_err(|_| ProviderError::InvalidRequest)?;
        if bytes.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        let credential = CredentialResolver::new(self.store).resolve(
            self.config.profile.provider(),
            &self.config.profile,
            cancel,
        )?;
        let headers = [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            (
                "x-rapidlm-request-id".to_owned(),
                req.request_id().as_str().to_owned(),
            ),
        ];
        let outbound = ProviderHttpRequest::new(&url, &headers, &bytes, &credential)
            .with_auth_style(if self.config.endpoint.is_vertex() {
                ProviderAuthStyle::Bearer
            } else {
                ProviderAuthStyle::GoogleApiKey
            });
        let response = if let Some(sink) = sink {
            let mut live = LiveText::default();
            let response = self
                .transport
                .execute_streaming(&outbound, cancel, &mut |chunk| live.feed(chunk, sink))?;
            classify_http_error(&response)?;
            if live.failed {
                return Err(ProviderError::Permanent);
            }
            response
        } else {
            self.transport.execute(&outbound, cancel)?
        };
        drop(credential);
        classify_http_error(&response)?;
        parse_google_response(&req, response.body(), &self.config.profile, &url, cancel)
    }
}
impl<T: HttpTransport> ProviderAdapter for GoogleGenAiAdapter<'_, T> {
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

pub fn encode_google_payload(
    req: &CanonicalModelRequest,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    cancel.check()?;
    let mut system = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // A result belongs to the call's name and original optional wire id, not a guessed name.
    let mut pending: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    for message in req.messages() {
        cancel.check()?;
        if message.role() == MessageRole::System {
            for part in message.parts() {
                match part {
                    ContentPart::Text { text } => system.push(json!({"text":text})),
                    _ => return Err(ProviderError::InvalidRequest),
                }
            }
            continue;
        }
        let role = if message.role() == MessageRole::Assistant {
            "model"
        } else {
            "user"
        };
        let parts = if message.role() == MessageRole::Tool {
            let id = message
                .tool_call_id()
                .ok_or(ProviderError::InvalidRequest)?
                .as_str();
            let (name, wire_id) = pending.remove(id).ok_or(ProviderError::InvalidRequest)?;
            let text: String = message
                .parts()
                .iter()
                .filter_map(|p| {
                    if let ContentPart::Text { text } = p {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut output = json!({"name":name,"response":{"output":text}});
            if let Some(id) = wire_id {
                output["id"] = id.into();
            }
            let media = message
                .parts()
                .iter()
                .filter(|p| !matches!(p, ContentPart::Text { .. }))
                .map(encode_part)
                .collect::<Result<Vec<_>, _>>()?;
            if !media.is_empty() {
                output["parts"] = media.into();
            }
            vec![json!({"functionResponse":output})]
        } else if message.role() == MessageRole::Assistant {
            if !pending.is_empty() {
                return Err(ProviderError::InvalidRequest);
            }
            let parts = encode_model_parts(message)?;
            for (index, call) in message.tool_calls().iter().enumerate() {
                if !google_name(call.name().as_str()) {
                    return Err(ProviderError::InvalidRequest);
                }
                let wire_id = original_id(message, index, call.call_id().as_str());
                if pending
                    .insert(
                        call.call_id().as_str().to_owned(),
                        (call.name().as_str().to_owned(), wire_id),
                    )
                    .is_some()
                {
                    return Err(ProviderError::InvalidRequest);
                }
            }
            parts
        } else {
            if !pending.is_empty() {
                return Err(ProviderError::InvalidRequest);
            }
            message
                .parts()
                .iter()
                .map(encode_part)
                .collect::<Result<Vec<_>, _>>()?
        };
        if parts.is_empty() {
            continue;
        }
        // Adjacent user/tool results make one content with ordered response parts.
        if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
            last["parts"]
                .as_array_mut()
                .ok_or(ProviderError::InvalidRequest)?
                .extend(parts);
        } else {
            contents.push(json!({"role":role,"parts":parts}));
        }
    }
    if contents.is_empty()
        || !pending.is_empty()
        || contents.first().is_none_or(|c| c["role"] != "user")
    {
        return Err(ProviderError::InvalidRequest);
    }
    let mut payload = json!({"contents":contents,"generationConfig":{"candidateCount":1,"responseModalities":["TEXT"]}});
    if !system.is_empty() {
        payload["systemInstruction"] = json!({"parts":system});
    }
    if let Some(max) = req.max_output_tokens() {
        payload["generationConfig"]["maxOutputTokens"] = max.into();
    }
    if let Some(effort) = req.reasoning_effort() {
        let config = match effort {
            ReasoningEffort::None => json!({"thinkingBudget":0}),
            ReasoningEffort::Minimal => json!({"thinkingLevel":"MINIMAL"}),
            ReasoningEffort::Low => json!({"thinkingLevel":"LOW"}),
            ReasoningEffort::Medium => json!({"thinkingLevel":"MEDIUM"}),
            ReasoningEffort::High => json!({"thinkingLevel":"HIGH"}),
            _ => return Err(ProviderError::InvalidRequest),
        };
        payload["generationConfig"]["thinkingConfig"] = config;
    }
    if !req.tools().is_empty() {
        let mut declarations = Vec::new();
        for tool in req.tools() {
            if !google_name(tool.name().as_str()) {
                return Err(ProviderError::InvalidRequest);
            }
            declarations.push(json!({"name":tool.name().as_str(),"description":tool.description(),"parametersJsonSchema":tool.parameters()}));
        }
        payload["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    Ok(payload)
}
fn encode_part(part: &ContentPart) -> Result<Value, ProviderError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"text":text})),
        ContentPart::ImageData { data_url } => {
            let (prefix, data) = data_url
                .split_once(";base64,")
                .ok_or(ProviderError::InvalidRequest)?;
            let mime = prefix
                .strip_prefix("data:")
                .filter(|mime| {
                    matches!(
                        *mime,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    )
                })
                .ok_or(ProviderError::InvalidRequest)?;
            Ok(json!({"inlineData":{"mimeType":mime,"data":data}}))
        }
        // An internal artifact id is not a Google file URI. Resolve it before this boundary.
        ContentPart::Image { .. } => Err(ProviderError::InvalidRequest),
    }
}
fn original_id(message: &CanonicalMessage, index: usize, fallback: &str) -> Option<String> {
    message
        .provider_replay()
        .and_then(|state| {
            state.items().iter().find_map(|item| match &item.data {
                ReplayData::GoogleCall { wire_id, .. } if item.before_call == index => {
                    Some(wire_id.clone())
                }
                _ => None,
            })
        })
        .unwrap_or_else(|| Some(fallback.to_owned()))
}
fn call_part(
    message: &CanonicalMessage,
    index: usize,
    signature: Option<&str>,
) -> Result<Value, ProviderError> {
    let call = message
        .tool_calls()
        .get(index)
        .ok_or(ProviderError::InvalidRequest)?;
    let args: Value =
        serde_json::from_str(call.arguments()).map_err(|_| ProviderError::InvalidRequest)?;
    if !args.is_object() || !google_name(call.name().as_str()) {
        return Err(ProviderError::InvalidRequest);
    }
    let mut part = json!({"functionCall":{"name":call.name().as_str(),"args":args}});
    if let Some(id) = original_id(message, index, call.call_id().as_str()) {
        part["functionCall"]["id"] = id.into();
    }
    if let Some(sig) = signature {
        part["thoughtSignature"] = sig.into();
    }
    Ok(part)
}
fn encode_model_parts(message: &CanonicalMessage) -> Result<Vec<Value>, ProviderError> {
    let Some(state) = message.provider_replay() else {
        let mut parts = message
            .parts()
            .iter()
            .map(encode_part)
            .collect::<Result<Vec<_>, _>>()?;
        for index in 0..message.tool_calls().len() {
            parts.push(call_part(message, index, None)?);
        }
        return Ok(parts);
    };
    if state.protocol() != ReplayProtocol::GoogleGenAi {
        return Err(ProviderError::InvalidRequest);
    }
    let mut parts = Vec::new();
    let mut calls = BTreeSet::new();
    // Text is already present in the private wire snapshot. For a length continuation,
    // prepend only earlier accumulated answer text, not this response's text twice.
    let visible: String = state
        .items()
        .iter()
        .filter_map(|item| match &item.data {
            ReplayData::GoogleText {
                text: Some(text),
                thought: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let requested: String = message
        .parts()
        .iter()
        .filter_map(|part| {
            if let ContentPart::Text { text } = part {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    if !requested.is_empty() && requested != visible {
        let prefix = requested
            .strip_suffix(&visible)
            .ok_or(ProviderError::InvalidRequest)?;
        if !prefix.is_empty() {
            parts.push(json!({"text":prefix}));
        }
    }
    for item in state.items() {
        match &item.data {
            ReplayData::GoogleText {
                text,
                thought,
                signature,
            } => {
                let mut part = json!({});
                if let Some(text) = text {
                    part["text"] = text.as_str().into();
                }
                if *thought {
                    part["thought"] = true.into();
                }
                if let Some(signature) = signature {
                    part["thoughtSignature"] = signature.as_str().into();
                }
                parts.push(part);
            }
            ReplayData::GoogleCall { signature, .. } => {
                if !calls.insert(item.before_call) {
                    return Err(ProviderError::InvalidRequest);
                }
                parts.push(call_part(message, item.before_call, signature.as_deref())?);
            }
            _ => return Err(ProviderError::InvalidRequest),
        }
    }
    if calls.len() != message.tool_calls().len() {
        return Err(ProviderError::InvalidRequest);
    }
    Ok(parts)
}
fn google_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn frames(body: &[u8]) -> Result<Vec<Value>, ProviderError> {
    if body.len() > MAX_HTTP_RESPONSE_BYTES {
        return Err(ProviderError::Permanent);
    }
    let text = std::str::from_utf8(body).map_err(|_| ProviderError::Permanent)?;
    if text.trim_start().starts_with('{') {
        return Ok(vec![
            serde_json::from_str(text).map_err(|_| ProviderError::Permanent)?,
        ]);
    }
    let blocks = sse_data_blocks(text);
    if blocks.is_empty() {
        return Err(ProviderError::Permanent);
    }
    blocks
        .into_iter()
        .filter(|s| s != "[DONE]")
        .map(|s| serde_json::from_str(&s).map_err(|_| ProviderError::Permanent))
        .collect()
}
fn candidate(frame: &Value) -> Result<Option<&Value>, ProviderError> {
    if !frame.is_object()
        || frame.get("error").is_some()
        || frame
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            .is_some_and(|s| s != "BLOCK_REASON_UNSPECIFIED")
    {
        return Err(ProviderError::Permanent);
    }
    let Some(candidates) = frame.get("candidates") else {
        return Ok(None);
    };
    let candidates = candidates.as_array().ok_or(ProviderError::Permanent)?;
    if candidates.len() > 1 {
        return Err(ProviderError::Permanent);
    }
    let Some(candidate) = candidates.first() else {
        return Ok(None);
    };
    if candidate
        .get("index")
        .is_some_and(|i| i.as_u64() != Some(0))
        || candidate
            .pointer("/content/role")
            .is_some_and(|r| r != "model")
    {
        return Err(ProviderError::Permanent);
    }
    if candidate
        .get("finishReason")
        .is_some_and(|r| !matches!(r.as_str(), Some("STOP" | "MAX_TOKENS")))
    {
        return Err(ProviderError::Permanent);
    }
    Ok(Some(candidate))
}
fn optional_string(value: &Value, key: &str) -> Result<Option<String>, ProviderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(ProviderError::Permanent),
    }
}
fn parse_google_response(
    req: &CanonicalModelRequest,
    body: &[u8],
    profile: &ProviderProfile,
    url: &str,
    cancel: &CancellationToken,
) -> Result<ModelStream, ProviderError> {
    cancel.check()?;
    let mut events = Vec::new();
    let mut calls = Vec::new();
    let mut items = Vec::new();
    let mut finish = None;
    let mut usage = Map::new();
    let mut ids = BTreeSet::new();
    let mut replay_bytes = 0usize;
    for frame in frames(body)? {
        cancel.check()?;
        if let Some(next) = frame.get("usageMetadata") {
            for (key, value) in next.as_object().ok_or(ProviderError::Permanent)? {
                usage.insert(key.clone(), value.clone());
            }
        }
        let Some(candidate) = candidate(&frame)? else {
            continue;
        };
        if let Some(reason) = candidate.get("finishReason") {
            finish = Some(reason.as_str().ok_or(ProviderError::Permanent)?.to_owned());
        }
        if let Some(parts) = candidate.pointer("/content/parts") {
            for part in parts.as_array().ok_or(ProviderError::Permanent)? {
                cancel.check()?;
                let signature = optional_string(part, "thoughtSignature")?;
                if let Some(fc) = part.get("functionCall") {
                    if part.get("text").is_some() || part.get("thought").is_some_and(|v| v != false)
                    {
                        return Err(ProviderError::Permanent);
                    }
                    let name = fc
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| google_name(name))
                        .ok_or(ProviderError::Permanent)?;
                    let wire_id = optional_string(fc, "id")?;
                    let internal_id = wire_id.clone().unwrap_or_else(|| {
                        format!(
                            "g{}_{}",
                            &scope_key(&[req.request_id().as_str()])[..16],
                            calls.len()
                        )
                    });
                    let call_id =
                        ToolCallId::parse(&internal_id).map_err(|_| ProviderError::Permanent)?;
                    if !ids.insert(internal_id.clone()) || calls.len() >= crate::MAX_TOOL_CALLS {
                        return Err(ProviderError::Permanent);
                    }
                    if fc.get("partialArgs").is_some() || fc.get("willContinue").is_some() {
                        return Err(ProviderError::Permanent);
                    }
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    if !args.is_object() {
                        return Err(ProviderError::Permanent);
                    }
                    let args =
                        serde_json::to_string(&args).map_err(|_| ProviderError::Permanent)?;
                    let call = ReplayCall::new(&internal_id, name, &args)
                        .map_err(|_| ProviderError::Permanent)?;
                    push_event(
                        &mut events,
                        ModelStreamEvent::ToolCallStart {
                            call_id: call_id.clone(),
                            name: ToolName::parse(name).map_err(|_| ProviderError::Permanent)?,
                        },
                    )?;
                    push_argument_deltas(&mut events, call_id, &args)?;
                    replay_bytes += signature.as_ref().map_or(0, String::len) + internal_id.len();
                    items.push(ReplayItem {
                        before_call: calls.len(),
                        data: ReplayData::GoogleCall {
                            call_id: internal_id,
                            wire_id,
                            signature,
                        },
                    });
                    calls.push(call);
                } else {
                    if part.as_object().is_none_or(|obj| {
                        obj.keys().any(|key| {
                            !matches!(key.as_str(), "text" | "thought" | "thoughtSignature")
                        })
                    }) {
                        return Err(ProviderError::Permanent);
                    }
                    let text = optional_string(part, "text")?;
                    let thought = match part.get("thought") {
                        None => false,
                        Some(Value::Bool(b)) => *b,
                        _ => return Err(ProviderError::Permanent),
                    };
                    if text.is_none() && signature.is_none() {
                        return Err(ProviderError::Permanent);
                    }
                    if !thought && let Some(text) = &text {
                        push_text_deltas(&mut events, text)?;
                    }
                    replay_bytes += text.as_ref().map_or(0, String::len)
                        + signature.as_ref().map_or(0, String::len);
                    // Unsigned text chunks can be coalesced; signed boundaries are exact.
                    if signature.is_none()
                        && let Some(ReplayItem {
                            before_call,
                            data:
                                ReplayData::GoogleText {
                                    text: Some(prior),
                                    thought: prior_thought,
                                    signature: None,
                                },
                        }) = items.last_mut()
                        && *before_call == calls.len()
                        && *prior_thought == thought
                        && let Some(text) = &text
                    {
                        prior.push_str(text);
                    } else {
                        items.push(ReplayItem {
                            before_call: calls.len(),
                            data: ReplayData::GoogleText {
                                text,
                                thought,
                                signature,
                            },
                        });
                    }
                }
                if replay_bytes > MAX_REPLAY_BYTES || items.len() > MAX_REPLAY_ITEMS {
                    return Err(ProviderError::Permanent);
                }
            }
        }
    }
    let finish = match finish.as_deref() {
        Some("STOP") if !calls.is_empty() => FinishReason::ToolCalls,
        Some("STOP") => FinishReason::Stop,
        Some("MAX_TOKENS") if calls.is_empty() => FinishReason::Length,
        _ => return Err(ProviderError::Permanent),
    };
    let usage = normalize_usage(&usage)?;
    push_event(&mut events, ModelStreamEvent::Completed { finish, usage })?;
    let stream = ModelStream::from_events(
        req.request_id().clone(),
        req.model().clone(),
        events,
        cancel,
    )?;
    if items.is_empty() {
        return Ok(stream);
    }
    let state = ProviderReplayState::new(
        ReplayProtocol::GoogleGenAi,
        profile.provider().as_str(),
        req.model().model().as_str(),
        &super::replay::scope(profile, url),
        calls,
        items,
    )
    .map_err(|_| ProviderError::Permanent)?;
    stream.with_provider_replay(Some(state))
}
fn normalize_usage(usage: &Map<String, Value>) -> Result<NormalizedUsage, ProviderError> {
    let read = |key: &str| -> Result<Option<u64>, ProviderError> {
        match usage.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or(ProviderError::Permanent),
        }
    };
    let prompt = read("promptTokenCount")?;
    let cached = read("cachedContentTokenCount")?;
    let tool = read("toolUsePromptTokenCount")?;
    let thoughts = read("thoughtsTokenCount")?;
    let candidates = read("candidatesTokenCount")?;
    let add = |a: Option<u64>, b: Option<u64>| -> Result<Option<u64>, ProviderError> {
        if a.is_none() && b.is_none() {
            Ok(None)
        } else {
            a.unwrap_or(0)
                .checked_add(b.unwrap_or(0))
                .map(Some)
                .ok_or(ProviderError::Permanent)
        }
    };
    let input = add(prompt, tool)?;
    let output = add(candidates, thoughts)?;
    if cached
        .zip(input)
        .is_some_and(|(cache, input)| cache > input)
    {
        return Err(ProviderError::Permanent);
    }
    let mut normalized = NormalizedUsage::new(
        input,
        cached,
        input.map(|input| input.saturating_sub(cached.unwrap_or(0))),
        output,
        thoughts,
        tool,
        UsageCost::Unknown,
    );
    if let Some(total) = read("totalTokenCount")? {
        normalized.insert_extra("totalTokenCount", UsageExtValue::U64(total))?;
    }
    Ok(normalized)
}
#[derive(Default)]
struct LiveText {
    pending: String,
    failed: bool,
}
impl LiveText {
    fn feed(&mut self, chunk: &str, sink: &mut dyn FnMut(&str)) {
        if self.failed {
            return;
        }
        self.pending.push_str(chunk);
        if self.pending.len() > MAX_HTTP_RESPONSE_BYTES {
            self.failed = true;
            return;
        }
        loop {
            let end = match (self.pending.find("\n\n"), self.pending.find("\r\n\r\n")) {
                (Some(a), Some(b)) => Some(if a < b { (a, 2) } else { (b, 4) }),
                (Some(a), None) => Some((a, 2)),
                (None, Some(b)) => Some((b, 4)),
                _ => None,
            };
            let Some((at, width)) = end else {
                break;
            };
            let block: String = self.pending.drain(..at + width).collect();
            for data in sse_data_blocks(&block) {
                if data == "[DONE]" {
                    continue;
                }
                let result = (|| -> Result<(), ProviderError> {
                    let frame: Value =
                        serde_json::from_str(&data).map_err(|_| ProviderError::Permanent)?;
                    if let Some(candidate) = candidate(&frame)?
                        && let Some(parts) = candidate.pointer("/content/parts")
                    {
                        for part in parts.as_array().ok_or(ProviderError::Permanent)? {
                            if part.get("thought").is_none_or(|b| b == false)
                                && let Some(text) = part.get("text").and_then(Value::as_str)
                            {
                                sink(text);
                            }
                        }
                    }
                    Ok(())
                })();
                if result.is_err() {
                    self.failed = true;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CanonicalToolSpec, CatalogRevision, ModelId, ModelPurpose, ModelRef, ModelRequestId,
        ProviderId, ToolCall,
    };
    use protocol::{Baggage, TraceContext, TraceId};
    fn profile() -> ProviderProfile {
        ProviderProfile::new(
            "work",
            ProviderId::parse("google-genai").unwrap(),
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
        )
        .unwrap()
    }
    fn req() -> CanonicalModelRequest {
        CanonicalModelRequest::new(ModelRequestId::parse("req-google-1").unwrap(),ModelRef::new(profile().provider().clone(),ModelId::parse("gemini-fixture").unwrap()),ModelPurpose::Code,
            vec![CanonicalMessage::new(MessageRole::System,vec![ContentPart::text("system instructions").unwrap()],None,vec![]).unwrap(),CanonicalMessage::new(MessageRole::User,vec![ContentPart::text("task").unwrap()],None,vec![]).unwrap()],
            vec![CanonicalToolSpec::new(ToolName::parse("read").unwrap(),"read a file",json!({"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false})).unwrap()],Some(512),CatalogRevision::new(1).unwrap(),TraceContext::new(TraceId::new(),None,Baggage::empty()),&CancellationToken::new()).unwrap()
    }
    fn response(parts: Value) -> Value {
        json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"cachedContentTokenCount":5,"candidatesTokenCount":6,"thoughtsTokenCount":3,"toolUsePromptTokenCount":2,"totalTokenCount":31}})
    }
    fn parse(request: &CanonicalModelRequest, body: &Value) -> Result<ModelStream, ProviderError> {
        parse_google_response(
            request,
            body.to_string().as_bytes(),
            &profile(),
            "https://example.test/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse",
            &CancellationToken::new(),
        )
    }
    fn assistant(stream: &ModelStream) -> CanonicalMessage {
        let calls = stream
            .events()
            .iter()
            .filter_map(|event| {
                if let ModelStreamEvent::ToolCallStart { call_id, name } = event {
                    let args: String = stream
                        .events()
                        .iter()
                        .filter_map(|e| {
                            if let ModelStreamEvent::ToolCallArgumentsDelta {
                                call_id: id,
                                arguments_delta,
                            } = e
                            {
                                if id == call_id {
                                    Some(arguments_delta.as_str())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        })
                        .collect();
                    Some(ToolCall::new(call_id.clone(), name.clone(), args).unwrap())
                } else {
                    None
                }
            })
            .collect();
        CanonicalMessage::new(MessageRole::Assistant, vec![], None, calls)
            .unwrap()
            .with_provider_replay(stream.provider_replay().unwrap().clone())
            .unwrap()
    }
    #[test]
    fn google_native_payload_preserves_system_images_tools_bounds_and_explicit_effort() {
        let request = req();
        let mut messages = request.messages().to_vec();
        messages.push(
            CanonicalMessage::new(
                MessageRole::User,
                vec![ContentPart::image_data("data:image/png;base64,aGVsbG8=").unwrap()],
                None,
                vec![],
            )
            .unwrap(),
        );
        let request = request
            .with_messages(messages)
            .unwrap()
            .with_reasoning_effort(ReasoningEffort::Low);
        let wire = encode_google_payload(&request, &CancellationToken::new()).unwrap();
        assert_eq!(
            wire["systemInstruction"]["parts"][0]["text"],
            "system instructions"
        );
        assert_eq!(
            wire["contents"][0]["parts"][1]["inlineData"]["data"],
            "aGVsbG8="
        );
        assert_eq!(
            wire["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "LOW"
        );
        assert_eq!(wire["generationConfig"]["maxOutputTokens"], 512);
        assert_eq!(
            wire["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["additionalProperties"],
            false
        );
        for model in [
            "../evil",
            "gemini?key=x",
            "models/evil/path",
            "gemini#x",
            "gemini:bad",
            "",
        ] {
            assert!(
                GoogleGenAiEndpoint::new("https://example.test/v1beta")
                    .unwrap()
                    .request_url(model)
                    .is_err(),
                "{model}"
            );
        }
        assert_eq!(
            GoogleGenAiEndpoint::new("https://example.test/v1beta/")
                .unwrap()
                .request_url("models/gemini-fixture")
                .unwrap(),
            "https://example.test:443/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse"
        );
        for origin in [
            "http://169.254.169.254/v1beta",
            "http://u:p@example.test",
            "https://example.test/v1beta?key=secret",
        ] {
            assert!(GoogleGenAiEndpoint::new(origin).is_err());
        }
    }
    #[test]
    fn google_parallel_calls_and_signed_text_replay_exact_parts_and_original_optional_ids() {
        let request = req();
        let parts = json!([
            {"text":"private-thought","thought":true,"thoughtSignature":"think-signature"},
            {"text":"explaining","thoughtSignature":"text-signature"},
            {"functionCall":{"name":"read","args":{"path":"a.txt"}},"thoughtSignature":"call-signature"},
            {"functionCall":{"id":"call-z","name":"read","args":{"path":"b.txt"}}},
            {"thoughtSignature":"empty-final-signature"}
        ]);
        let body = response(parts.clone());
        let stream = parse(&request, &body).unwrap();
        let text: String = stream
            .events()
            .iter()
            .filter_map(|e| {
                if let ModelStreamEvent::TextDelta { text } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "explaining");
        assert!(!format!("{:?}", stream.provider_replay()).contains("private-thought"));
        let assistant = assistant(&stream);
        let restored: CanonicalMessage =
            serde_json::from_str(&serde_json::to_string(&assistant).unwrap()).unwrap();
        let mut messages = request.messages().to_vec();
        messages.push(restored.clone());
        for call in restored.tool_calls() {
            messages.push(
                CanonicalMessage::new(
                    MessageRole::Tool,
                    vec![
                        ContentPart::text("result").unwrap(),
                        ContentPart::image_data("data:image/jpeg;base64,aGVsbG8=").unwrap(),
                    ],
                    Some(call.call_id().clone()),
                    vec![],
                )
                .unwrap(),
            );
        }
        let request = request.with_messages(messages).unwrap();
        let wire = encode_google_payload(&request, &CancellationToken::new()).unwrap();
        assert_eq!(wire["contents"][1]["parts"], parts);
        let results = &wire["contents"][2]["parts"];
        assert!(results[0]["functionResponse"].get("id").is_none());
        assert_eq!(results[1]["functionResponse"]["id"], "call-z");
        assert_eq!(results[0]["functionResponse"]["name"], "read");
        assert_eq!(
            results[0]["functionResponse"]["parts"][0]["inlineData"]["mimeType"],
            "image/jpeg"
        );
        let usage = stream.terminal_usage().unwrap();
        assert_eq!(usage.input_tokens(), Some(22));
        assert_eq!(usage.output_tokens(), Some(9));
        assert_eq!(usage.cached_input_tokens(), Some(5));
        assert_eq!(usage.reasoning_tokens(), Some(3));
        assert!(
            super::super::replay::validate(
                &request,
                Some(ReplayProtocol::GoogleGenAi),
                &profile(),
                "https://other.test/v1beta"
            )
            .is_err()
        );
        let public = protocol::provider_replay::public_payload(
            &json!({"provider_replay":stream.provider_replay()}),
        );
        assert!(!public.to_string().contains("empty-final-signature"));
    }
    #[test]
    fn google_sse_coalesces_unsigned_deltas_and_preserves_signature_only_tail() {
        let request = req();
        let mut wire = String::new();
        let mut live = LiveText::default();
        let mut visible = String::new();
        for part in [
            json!({"text":"hidden","thought":true}),
            json!({"text":"🐈"}),
        ]
        .into_iter()
        .chain(std::iter::repeat_n(json!({"text":"a"}), 2000))
        .chain([json!({"text":"","thoughtSignature":"last-signature"})])
        {
            let frame = json!({"candidates":[{"content":{"role":"model","parts":[part]}}]});
            let data = format!("data: {frame}\r\n\r\n");
            for ch in data.chars() {
                live.feed(&ch.to_string(), &mut |s| visible.push_str(s));
            }
            wire.push_str(&data);
        }
        wire.push_str(&format!(
            "data: {}\r\n\r\n",
            json!({"candidates":[{"finishReason":"STOP"}]})
        ));
        let stream = parse_google_response(
            &request,
            wire.as_bytes(),
            &profile(),
            "https://example.test",
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(visible, format!("🐈{}", "a".repeat(2000)));
        assert!(!live.failed);
        let items = stream.provider_replay().unwrap().items();
        assert_eq!(items.len(), 3);
        assert!(
            matches!(&items[2].data,ReplayData::GoogleText {text:Some(text),signature:Some(sig),..} if text.is_empty()&&sig=="last-signature")
        );
    }
    #[test]
    fn google_blocks_malformed_or_incomplete_provider_output_before_tools_can_execute() {
        let request = req();
        for mut body in [
            response(
                json!([{"functionCall":{"id":"same","name":"read","args":{}}},{"functionCall":{"id":"same","name":"read","args":{}}}]),
            ),
            response(json!([{"functionCall":{"name":"../invalid","args":{}}}])),
            response(json!([{"functionCall":{"name":"read","args":[]}}])),
            response(json!([{"inlineData":{"mimeType":"image/png","data":"a"}}])),
            response(json!([{"text":"private","thought":"true"}])),
            response(json!([{"thoughtSignature":"x".repeat(MAX_REPLAY_BYTES+1)}])),
        ] {
            assert!(parse(&request, &body).is_err());
            body["candidates"][0]["finishReason"] = "SAFETY".into();
            assert!(parse(&request, &body).is_err());
        }
        for reason in [
            "SAFETY",
            "RECITATION",
            "MALFORMED_FUNCTION_CALL",
            "OTHER",
            "MISSING_THOUGHT_SIGNATURE",
            "FINISH_REASON_UNSPECIFIED",
        ] {
            let mut body = response(json!([{"text":"not successful"}]));
            body["candidates"][0]["finishReason"] = reason.into();
            assert_eq!(parse(&request, &body), Err(ProviderError::Permanent));
        }
        let mut truncated = response(json!([{"functionCall":{"name":"read","args":{}}}]));
        truncated["candidates"][0]["finishReason"] = "MAX_TOKENS".into();
        assert!(parse(&request, &truncated).is_err());
        let mut incomplete = response(json!([{"text":"partial"}]));
        incomplete["candidates"][0]
            .as_object_mut()
            .unwrap()
            .remove("finishReason");
        assert!(parse(&request, &incomplete).is_err());
        assert!(
            parse(
                &request,
                &json!({"promptFeedback":{"blockReason":"SAFETY"}})
            )
            .is_err()
        );
        let mut multi = response(json!([{"text":"x"}]));
        multi["candidates"]
            .as_array_mut()
            .unwrap()
            .push(json!({"index":1}));
        assert!(parse(&request, &multi).is_err());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            parse_google_response(&request, b"{}", &profile(), "https://example.test", &cancel),
            Err(ProviderError::Cancelled)
        );
    }
}
