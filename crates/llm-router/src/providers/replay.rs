//! Bounded private continuation conversion. Never feed these blocks to text sinks.
use crate::{CanonicalModelRequest, ModelStream, ModelStreamEvent, ProviderError, ProviderProfile};
use protocol::provider_replay::{
    ProviderReplayState, ReplayCall, ReplayData, ReplayItem, ReplayProtocol, scope_key,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn scope(profile: &ProviderProfile, endpoint: &str) -> String {
    // Store-assigned ids change after restart; a configured alias is stable.
    let handle = profile
        .credential()
        .alias()
        .or(profile.credential().id())
        .unwrap_or_default();
    scope_key(&[
        profile.id().as_str(),
        profile.provider().as_str(),
        endpoint.trim_end_matches('/'),
        handle,
    ])
}

pub fn matches(
    state: &ProviderReplayState,
    protocol: ReplayProtocol,
    profile: &ProviderProfile,
    endpoint: &str,
    model: &str,
) -> bool {
    state.matches_scope(
        protocol,
        profile.provider().as_str(),
        model,
        &scope(profile, endpoint),
    )
}

pub(crate) fn validate(
    req: &CanonicalModelRequest,
    protocol: Option<ReplayProtocol>,
    profile: &ProviderProfile,
    endpoint: &str,
) -> Result<(), ProviderError> {
    for message in req.messages() {
        if let Some(state) = message.provider_replay()
            && !protocol
                .is_some_and(|p| matches(state, p, profile, endpoint, req.model().model().as_str()))
        {
            return Err(ProviderError::InvalidRequest);
        }
    }
    Ok(())
}

pub(crate) fn bindings(stream: &ModelStream) -> Result<Vec<ReplayCall>, ProviderError> {
    let mut calls: Vec<(String, String, String)> = Vec::new();
    for event in stream.events() {
        match event {
            ModelStreamEvent::ToolCallStart { call_id, name } => calls.push((
                call_id.as_str().to_owned(),
                name.as_str().to_owned(),
                String::new(),
            )),
            ModelStreamEvent::ToolCallArgumentsDelta {
                call_id,
                arguments_delta,
            } => {
                let call = calls
                    .iter_mut()
                    .find(|c| c.0 == call_id.as_str())
                    .ok_or(ProviderError::Permanent)?;
                call.2.push_str(arguments_delta);
            }
            _ => {}
        }
    }
    calls
        .iter()
        .map(|(id, name, args)| {
            ReplayCall::new(id, name, args).map_err(|_| ProviderError::Permanent)
        })
        .collect()
}

pub(crate) fn attach(
    stream: ModelStream,
    body: &[u8],
    protocol: ReplayProtocol,
    profile: &ProviderProfile,
    endpoint: &str,
) -> Result<ModelStream, ProviderError> {
    let body = std::str::from_utf8(body).map_err(|_| ProviderError::Permanent)?;
    let blocks = match protocol {
        ReplayProtocol::Responses => responses_blocks(body)?,
        ReplayProtocol::Anthropic => anthropic_blocks(body)?,
        ReplayProtocol::GoogleGenAi | ReplayProtocol::BedrockConverse => {
            return Err(ProviderError::InvalidRequest);
        }
    };
    let calls = bindings(&stream)?;
    let mut call_index = 0;
    let mut items = Vec::new();
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = match (protocol, kind) {
            (ReplayProtocol::Responses, "function_call")
            | (ReplayProtocol::Anthropic, "tool_use") => {
                let id = block
                    .get(if protocol == ReplayProtocol::Responses {
                        "call_id"
                    } else {
                        "id"
                    })
                    .and_then(Value::as_str)
                    .ok_or(ProviderError::Permanent)?;
                if calls
                    .get(call_index)
                    .is_none_or(|call| call.call_id() != id)
                {
                    return Err(ProviderError::Permanent);
                }
                call_index += 1;
                continue;
            }
            (ReplayProtocol::Responses, "reasoning") => {
                let Some(encrypted) = block
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                else {
                    // No tool execution may depend on a reasoning block that cannot be replayed.
                    if !calls.is_empty() {
                        return Err(ProviderError::Permanent);
                    }
                    continue;
                };
                ReplayData::ResponsesReasoning {
                    id: field(&block, "id")?,
                    encrypted_content: encrypted.to_owned(),
                }
            }
            (ReplayProtocol::Anthropic, "thinking") => ReplayData::AnthropicThinking {
                thinking: field(&block, "thinking")?,
                signature: field(&block, "signature")?,
            },
            (ReplayProtocol::Anthropic, "redacted_thinking") => ReplayData::AnthropicRedacted {
                data: field(&block, "data")?,
            },
            _ => continue,
        };
        items.push(ReplayItem {
            before_call: call_index,
            data,
        });
    }
    if items.is_empty() {
        return Ok(stream);
    }
    if call_index != calls.len() {
        return Err(ProviderError::Permanent);
    }
    let state = ProviderReplayState::new(
        protocol,
        profile.provider().as_str(),
        stream.model().model().as_str(),
        &scope(profile, endpoint),
        calls,
        items,
    )
    .map_err(|_| ProviderError::Permanent)?;
    stream.with_provider_replay(Some(state))
}

fn field(value: &Value, key: &str) -> Result<String, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ProviderError::Permanent)
}

fn frames(body: &str) -> Result<Vec<Value>, ProviderError> {
    super::openai_compatible::sse_data_blocks(body)
        .into_iter()
        .filter(|s| s != "[DONE]")
        .map(|s| serde_json::from_str(&s).map_err(|_| ProviderError::Permanent))
        .collect()
}
fn responses_blocks(body: &str) -> Result<Vec<Value>, ProviderError> {
    if body.trim_start().starts_with('{') {
        let root: Value = serde_json::from_str(body).map_err(|_| ProviderError::Permanent)?;
        return Ok(root
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default());
    }
    let mut blocks = BTreeMap::new();
    for frame in frames(body)? {
        match frame.get("type").and_then(Value::as_str) {
            Some("response.completed" | "response.incomplete") => {
                if let Some(output) = frame.pointer("/response/output").and_then(Value::as_array) {
                    return Ok(output.clone());
                }
            }
            Some("response.output_item.added" | "response.output_item.done") => {
                if let (Some(index), Some(item)) = (
                    frame.get("output_index").and_then(Value::as_u64),
                    frame.get("item"),
                ) {
                    blocks.insert(index, item.clone());
                }
            }
            _ => {}
        }
    }
    Ok(blocks.into_values().collect())
}
fn anthropic_blocks(body: &str) -> Result<Vec<Value>, ProviderError> {
    if body.trim_start().starts_with('{') {
        let root: Value = serde_json::from_str(body).map_err(|_| ProviderError::Permanent)?;
        return Ok(root
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default());
    }
    let mut blocks = BTreeMap::new();
    for frame in frames(body)? {
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = frame.get("index").and_then(Value::as_u64);
        if kind == "content_block_start" {
            if let (Some(index), Some(block)) = (index, frame.get("content_block")) {
                blocks.insert(index, block.clone());
            }
        } else if kind == "content_block_delta"
            && let Some(block) = index.and_then(|i| blocks.get_mut(&i))
        {
            let delta = &frame["delta"];
            let key = match delta["type"].as_str() {
                Some("thinking_delta") => "thinking",
                Some("signature_delta") => "signature",
                _ => continue,
            };
            let addition = field(delta, key)?;
            let prior = block.get(key).and_then(Value::as_str).unwrap_or_default();
            let combined = format!("{prior}{addition}");
            if combined.len() > protocol::provider_replay::MAX_REPLAY_BYTES {
                return Err(ProviderError::Permanent);
            }
            block[key] = combined.into();
        }
    }
    Ok(blocks.into_values().collect())
}

pub(crate) fn wire_items(
    state: Option<&ProviderReplayState>,
    position: usize,
    protocol: ReplayProtocol,
) -> Result<Vec<Value>, ProviderError> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    if state.protocol() != protocol {
        return Err(ProviderError::InvalidRequest);
    }
    state.items().iter().filter(|item| item.before_call == position).map(|item| match &item.data {
        ReplayData::ResponsesReasoning { id, encrypted_content } => Ok(json!({"type":"reasoning", "id":id, "summary":[], "encrypted_content":encrypted_content})),
        ReplayData::AnthropicThinking { thinking, signature } => Ok(json!({"type":"thinking", "thinking":thinking, "signature":signature})),
        ReplayData::AnthropicRedacted { data } => Ok(json!({"type":"redacted_thinking", "data":data})),
        _ => Err(ProviderError::InvalidRequest),
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CancellationToken, CanonicalMessage, MessageRole, ModelId, ModelRef, ModelRequestId,
        ProviderId, ToolCall, ToolCallId, ToolName,
    };
    fn profile(id: &str) -> ProviderProfile {
        ProviderProfile::new(
            "work",
            ProviderId::parse("test").unwrap(),
            auth::SecretRef::from_id_and_alias(id, "env:TEST_KEY").unwrap(),
        )
        .unwrap()
    }
    fn stream() -> ModelStream {
        ModelStream::from_events(
            ModelRequestId::parse("req1").unwrap(),
            ModelRef::new(
                ProviderId::parse("test").unwrap(),
                ModelId::parse("model").unwrap(),
            ),
            vec![
                ModelStreamEvent::ToolCallStart {
                    call_id: ToolCallId::parse("call1").unwrap(),
                    name: ToolName::parse("read").unwrap(),
                },
                ModelStreamEvent::ToolCallArgumentsDelta {
                    call_id: ToolCallId::parse("call1").unwrap(),
                    arguments_delta: "{}".to_owned(),
                },
            ],
            &CancellationToken::new(),
        )
        .unwrap()
    }
    #[test]
    fn responses_private_items_round_trip_in_order_with_scope_and_call_binding() {
        let p = profile("01234567-89ab-cdef-0123-456789abcdef");
        let output = json!([
            {"type":"reasoning","id":"rs1","encrypted_content":"private-first"},
            {"type":"function_call","call_id":"call1"},
            {"type":"reasoning","id":"rs2","encrypted_content":"private-last"}
        ]);
        for body in [
            json!({"output":output}).to_string(),
            format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_item.done", "output_index":0,"item":output[0]}),
                json!({"type":"response.output_item.done","output_index":1,"item":output[1]}),
                json!({"type":"response.output_item.done","output_index":2,"item":output[2]})
            ),
        ] {
            let stream = attach(
                stream(),
                body.as_bytes(),
                ReplayProtocol::Responses,
                &p,
                "https://example.test/v1/responses",
            )
            .unwrap();
            let state = stream.provider_replay().unwrap();
            assert_eq!(
                state
                    .items()
                    .iter()
                    .map(|i| i.before_call)
                    .collect::<Vec<_>>(),
                [0, 1]
            );
            assert!(!format!("{stream:?}").contains("private-first"));
            assert!(matches(
                state,
                ReplayProtocol::Responses,
                &p,
                "https://example.test/v1/responses",
                "model"
            ));
            assert!(!matches(
                state,
                ReplayProtocol::Responses,
                &p,
                "https://other.test/v1/responses",
                "model"
            ));
            assert!(!matches(
                state,
                ReplayProtocol::Responses,
                &p,
                "https://example.test/v1/responses",
                "other-model"
            ));
            let restarted = profile("11234567-89ab-cdef-0123-456789abcdef");
            assert_eq!(
                scope(&p, "https://example.test/v1/responses"),
                scope(&restarted, "https://example.test/v1/responses")
            );
            let message = CanonicalMessage::new(
                MessageRole::Assistant,
                vec![],
                None,
                vec![
                    ToolCall::new(
                        ToolCallId::parse("call1").unwrap(),
                        ToolName::parse("read").unwrap(),
                        "{}",
                    )
                    .unwrap(),
                ],
            )
            .unwrap()
            .with_provider_replay(state.clone())
            .unwrap();
            let items = super::super::openai_compatible::encode_responses_items(&message).unwrap();
            assert_eq!(items[0]["encrypted_content"], "private-first");
            assert_eq!(items[1]["call_id"], "call1");
            assert_eq!(items[2]["encrypted_content"], "private-last");
            let text_message = CanonicalMessage::new(
                MessageRole::Assistant,
                vec![crate::ContentPart::text("explaining the call").unwrap()],
                None,
                message.tool_calls().to_vec(),
            )
            .unwrap()
            .with_provider_replay(state.clone())
            .unwrap();
            let ordered =
                super::super::openai_compatible::encode_responses_items(&text_message).unwrap();
            assert_eq!(ordered[0]["type"], "reasoning");
            assert_eq!(ordered[1]["type"], "message");
            assert_eq!(ordered[2]["type"], "function_call");
            assert_eq!(ordered[3]["encrypted_content"], "private-last");
            let bad = json!({"output":[{"type":"reasoning","id":"rs1"},{"type":"function_call","call_id":"call1"}]});
            assert!(
                attach(
                    self::stream(),
                    bad.to_string().as_bytes(),
                    ReplayProtocol::Responses,
                    &p,
                    "https://example.test"
                )
                .is_err()
            );
        }
    }
    #[test]
    fn anthropic_signed_thinking_is_reassembled_privately_including_redacted_blocks() {
        let p = profile("01234567-89ab-cdef-0123-456789abcdef");
        let frames = [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private-thought"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"nature"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque-redacted"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call1"}}),
        ];
        let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        let stream = attach(
            stream(),
            body.as_bytes(),
            ReplayProtocol::Anthropic,
            &p,
            "https://example.test/messages",
        )
        .unwrap();
        let blocks = wire_items(stream.provider_replay(), 0, ReplayProtocol::Anthropic).unwrap();
        assert_eq!(
            blocks[0],
            json!({"type":"thinking","thinking":"private-thought","signature":"signature"})
        );
        assert_eq!(
            blocks[1],
            json!({"type":"redacted_thinking","data":"opaque-redacted"})
        );
        assert!(!format!("{stream:?}").contains("private-thought"));
        assert!(
            !protocol::provider_replay::public_payload(
                &json!({"provider_replay":stream.provider_replay()})
            )
            .to_string()
            .contains("private-thought")
        );
    }
}
