//! Private provider continuation data, paired with the calls that produced it.
//!
//! This is durable protocol state, never model-visible context or answer text.
//! Debug omits its contents; public exports remove it. Provider adapters own
//! wire conversion, while this contract owns bounds and call/scope binding.

use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};
use std::fmt;

pub const MAX_REPLAY_BYTES: usize = 128 * 1024;
pub const MAX_REPLAY_ITEMS: usize = 64;
pub const MAX_REPLAY_CALLS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayProtocol {
    Responses,
    GoogleGenAi,
    Anthropic,
}

#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayData {
    ResponsesReasoning {
        id: String,
        encrypted_content: String,
    },
    GoogleSignature {
        call_id: String,
        signature: String,
    },
    GoogleCall {
        call_id: String,
        wire_id: Option<String>,
        signature: Option<String>,
    },
    GoogleText {
        text: Option<String>,
        thought: bool,
        signature: Option<String>,
    },
    AnthropicThinking {
        thinking: String,
        signature: String,
    },
    AnthropicRedacted {
        data: String,
    },
}

impl fmt::Debug for ReplayData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ResponsesReasoning { .. } => "ResponsesReasoning(<private>)",
            Self::GoogleSignature { .. } => "GoogleSignature(<private>)",
            Self::GoogleCall { .. } => "GoogleCall(<private>)",
            Self::GoogleText { .. } => "GoogleText(<private>)",
            Self::AnthropicThinking { .. } => "AnthropicThinking(<private>)",
            Self::AnthropicRedacted { .. } => "AnthropicRedacted(<private>)",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayItem {
    /// Insert immediately before this call's index; `calls.len()` means after
    /// the last call. Original provider ordering is retained.
    pub before_call: usize,
    pub data: ReplayData,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayCall {
    call_id: String,
    tool: String,
    arguments_sha256: String,
}

impl ReplayCall {
    pub fn new(call_id: &str, tool: &str, arguments: &str) -> Result<Self, ReplayError> {
        if !label(call_id, 128) || !label(tool, 128) || arguments.len() > 64 * 1024 {
            return Err(ReplayError);
        }
        Ok(Self {
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            arguments_sha256: scope_key(&[arguments]),
        })
    }
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
}

#[derive(Clone, Eq, PartialEq, Hash, Serialize)]
pub struct ProviderReplayState {
    schema_version: u16,
    protocol: ReplayProtocol,
    provider: String,
    model: String,
    scope: String,
    calls: Vec<ReplayCall>,
    items: Vec<ReplayItem>,
}

impl fmt::Debug for ProviderReplayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderReplayState")
            .field("protocol", &self.protocol)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("calls", &self.calls.len())
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayError;
impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid private provider continuation")
    }
}
impl std::error::Error for ReplayError {}

impl ProviderReplayState {
    pub fn new(
        protocol: ReplayProtocol,
        provider: &str,
        model: &str,
        scope: &str,
        calls: Vec<ReplayCall>,
        items: Vec<ReplayItem>,
    ) -> Result<Self, ReplayError> {
        let state = Self {
            schema_version: 1,
            protocol,
            provider: provider.to_owned(),
            model: model.to_owned(),
            scope: scope.to_owned(),
            calls,
            items,
        };
        state.validate()?;
        Ok(state)
    }
    fn validate(&self) -> Result<(), ReplayError> {
        if self.schema_version != 1
            || !label(&self.provider, 128)
            || !label(&self.model, 256)
            || !hex_digest(&self.scope)
            || self.calls.len() > MAX_REPLAY_CALLS
            || self.items.is_empty()
            || self.items.len() > MAX_REPLAY_ITEMS
        {
            return Err(ReplayError);
        }
        let mut ids = std::collections::BTreeSet::new();
        for call in &self.calls {
            if !label(&call.call_id, 128)
                || !label(&call.tool, 128)
                || !hex_digest(&call.arguments_sha256)
                || !ids.insert(&call.call_id)
            {
                return Err(ReplayError);
            }
        }
        let mut bytes = 0usize;
        let mut reasoning_ids = std::collections::BTreeSet::new();
        let mut signature_calls = std::collections::BTreeSet::new();
        let mut previous_position = 0;
        for item in &self.items {
            if item.before_call < previous_position {
                return Err(ReplayError);
            }
            previous_position = item.before_call;
            if item.before_call > self.calls.len() {
                return Err(ReplayError);
            }
            let size = match (&self.protocol, &item.data) {
                (
                    ReplayProtocol::Responses,
                    ReplayData::ResponsesReasoning {
                        id,
                        encrypted_content,
                    },
                ) if label(id, 256) && opaque(encrypted_content) && reasoning_ids.insert(id) => {
                    id.len() + encrypted_content.len()
                }
                (
                    ReplayProtocol::GoogleGenAi,
                    ReplayData::GoogleSignature { call_id, signature },
                ) if self
                    .calls
                    .get(item.before_call)
                    .is_some_and(|call| &call.call_id == call_id)
                    && opaque(signature)
                    && signature_calls.insert(call_id) =>
                {
                    call_id.len() + signature.len()
                }
                (
                    ReplayProtocol::GoogleGenAi,
                    ReplayData::GoogleCall {
                        call_id,
                        wire_id,
                        signature,
                    },
                ) if self
                    .calls
                    .get(item.before_call)
                    .is_some_and(|call| &call.call_id == call_id)
                    && wire_id.as_ref().is_none_or(|id| label(id, 128))
                    && signature.as_deref().is_none_or(opaque)
                    && signature_calls.insert(call_id) =>
                {
                    call_id.len()
                        + wire_id.as_ref().map_or(0, String::len)
                        + signature.as_ref().map_or(0, String::len)
                }
                (
                    ReplayProtocol::GoogleGenAi,
                    ReplayData::GoogleText {
                        text, signature, ..
                    },
                ) if (text.is_some() || signature.is_some())
                    && text.as_ref().is_none_or(|text| !text.contains('\0'))
                    && signature.as_deref().is_none_or(opaque) =>
                {
                    text.as_ref().map_or(0, String::len) + signature.as_ref().map_or(0, String::len)
                }
                (
                    ReplayProtocol::Anthropic,
                    ReplayData::AnthropicThinking {
                        thinking,
                        signature,
                    },
                ) if opaque(signature) && !thinking.contains('\0') => {
                    thinking.len() + signature.len()
                }
                (ReplayProtocol::Anthropic, ReplayData::AnthropicRedacted { data })
                    if opaque(data) =>
                {
                    data.len()
                }
                _ => return Err(ReplayError),
            };
            bytes = bytes.checked_add(size).ok_or(ReplayError)?;
            if bytes > MAX_REPLAY_BYTES {
                return Err(ReplayError);
            }
        }
        if serde_json::to_vec(self).map_err(|_| ReplayError)?.len() > MAX_REPLAY_BYTES {
            return Err(ReplayError);
        }
        Ok(())
    }
    pub fn protocol(&self) -> ReplayProtocol {
        self.protocol
    }
    pub fn items(&self) -> &[ReplayItem] {
        &self.items
    }
    pub fn calls(&self) -> &[ReplayCall] {
        &self.calls
    }
    pub fn matches_scope(
        &self,
        protocol: ReplayProtocol,
        provider: &str,
        model: &str,
        scope: &str,
    ) -> bool {
        self.protocol == protocol
            && self.provider == provider
            && self.model == model
            && self.scope == scope
    }
    pub fn matches_calls(&self, calls: &[ReplayCall]) -> bool {
        self.calls == calls
    }
    pub fn encoded_bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(MAX_REPLAY_BYTES, |bytes| bytes.len())
    }

    /// A validated batch prefix/subset may stop for approval. Keep metadata
    /// only with matching calls, remapping its positions without widening it.
    pub fn for_calls(&self, calls: &[ReplayCall]) -> Result<Option<Self>, ReplayError> {
        let mut positions = Vec::new();
        for call in calls {
            positions.push(
                self.calls
                    .iter()
                    .position(|old| old == call)
                    .ok_or(ReplayError)?,
            );
        }
        if positions.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ReplayError);
        }
        let items: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| {
                if let ReplayData::GoogleSignature { call_id, .. }
                | ReplayData::GoogleCall { call_id, .. } = &item.data
                    && !calls.iter().any(|call| &call.call_id == call_id)
                {
                    return None;
                }
                Some(ReplayItem {
                    before_call: positions.iter().filter(|p| **p < item.before_call).count(),
                    data: item.data.clone(),
                })
            })
            .collect();
        if items.is_empty() {
            return Ok(None);
        }
        Self::new(
            self.protocol,
            &self.provider,
            &self.model,
            &self.scope,
            calls.to_vec(),
            items,
        )
        .map(Some)
    }
}

impl<'de> Deserialize<'de> for ProviderReplayState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema_version: u16,
            protocol: ReplayProtocol,
            provider: String,
            model: String,
            scope: String,
            calls: Vec<ReplayCall>,
            items: Vec<ReplayItem>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let state = Self {
            schema_version: raw.schema_version,
            protocol: raw.protocol,
            provider: raw.provider,
            model: raw.model,
            scope: raw.scope,
            calls: raw.calls,
            items: raw.items,
        };
        state.validate().map_err(de::Error::custom)?;
        Ok(state)
    }
}

fn label(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}
fn opaque(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_REPLAY_BYTES
        && text.bytes().all(|b| (0x21..=0x7e).contains(&b))
}
fn hex_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Length-prefix every part: two scopes with ambiguous separators never alias.
pub fn scope_key(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// Sanitize a public payload, including the JSON string used by durable
/// approval suspensions. Internal ledger reads retain the original data.
pub fn public_payload(value: &serde_json::Value) -> serde_json::Value {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                object.remove("provider_replay");
                for (key, value) in object {
                    if key == "suspension"
                        && let Some(text) = value.as_str().filter(|s| s.contains("provider_replay"))
                    {
                        if let Ok(mut nested) = serde_json::from_str::<serde_json::Value>(text) {
                            strip(&mut nested);
                            *value = serde_json::Value::String(nested.to_string());
                        } else if text.contains("provider_replay") {
                            *value = serde_json::Value::Null;
                        }
                    } else {
                        strip(value);
                    }
                }
            }
            serde_json::Value::Array(array) => {
                for value in array {
                    strip(value);
                }
            }
            _ => {}
        }
    }
    let mut out = value.clone();
    strip(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> ProviderReplayState {
        ProviderReplayState::new(
            ReplayProtocol::Responses,
            "responses",
            "model",
            &scope_key(&["endpoint", "profile"]),
            vec![ReplayCall::new("call1", "read", "{}").unwrap()],
            vec![ReplayItem {
                before_call: 0,
                data: ReplayData::ResponsesReasoning {
                    id: "rs1".to_owned(),
                    encrypted_content: "opaque-canary".to_owned(),
                },
            }],
        )
        .unwrap()
    }
    #[test]
    fn replay_is_bounded_bound_to_calls_round_tripped_and_never_debugged() {
        let state = state();
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(
            serde_json::from_value::<ProviderReplayState>(raw.clone()).unwrap(),
            state
        );
        assert!(!format!("{state:?}").contains("opaque-canary"));
        assert!(!format!("{:?}", state.items()).contains("opaque-canary"));
        assert!(!state.matches_calls(&[ReplayCall::new("call1", "write", "{}").unwrap()]));
        for mutation in ["version", "extra", "size", "position", "protocol", "digest"] {
            let mut invalid = raw.clone();
            match mutation {
                "version" => invalid["schema_version"] = 2.into(),
                "extra" => invalid["unexpected"] = true.into(),
                "size" => {
                    invalid["items"][0]["data"]["encrypted_content"] =
                        "x".repeat(MAX_REPLAY_BYTES).into()
                }
                "position" => invalid["items"][0]["before_call"] = 2.into(),
                "protocol" => invalid["protocol"] = "google_gen_ai".into(),
                _ => invalid["calls"][0]["arguments_sha256"] = "bad".into(),
            }
            assert!(
                serde_json::from_value::<ProviderReplayState>(invalid).is_err(),
                "{mutation}"
            );
        }
    }
    #[test]
    fn public_exports_strip_nested_and_string_encoded_private_state() {
        let inner = serde_json::json!({"exchanges":[{"calls":[],"provider_replay":state()}]});
        let payload = serde_json::json!({"suspension": inner.to_string(), "detail":{"provider_replay":state()}, "other":"kept"});
        let exported = public_payload(&payload).to_string();
        assert!(!exported.contains("opaque-canary"));
        assert!(!exported.contains("provider_replay"));
        assert!(exported.contains("kept"));
        assert!(payload.to_string().contains("opaque-canary"));
    }
}
