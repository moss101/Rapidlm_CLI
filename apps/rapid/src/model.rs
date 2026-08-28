//! Composition bridge: user model config → llm-router adapter → `LiveModelCall`.
//!
//! The exec command's typed fallback ([`UnconfiguredModel`]) is replaced by a
//! [`ConfiguredModel`] when the Grok-style user config resolves a model. The
//! bridge owns:
//!
//!   - credential seeding: the resolved key is placed in an in-process
//!     [`InMemoryCredentialStore`] under a stable alias so the router's
//!     fail-closed [`CredentialResolver`] (metadata authorization + fetch)
//!     succeeds without any plaintext ever leaving the process;
//!   - transport selection: a bearer transport when a key is configured, and
//!     a no-auth transport for keyless local servers;
//!   - canonical request construction from the live [`ContextPacket`] blocks
//!     plus prior tool results, and typed folding of the provider stream back
//!     into [`ModelStepOutput`].
//!
//! The HTTP transport speaks HTTP/1.1 over plain TCP (local servers) or TLS
//! (https provider origins, verified against the static Mozilla root set;
//! there is no custom-CA or dynamic trust surface).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_runtime::{
    CancellationToken, FailureCause, ModelStepError, ModelStepInput, ModelStepOutput,
    ProposedToolCall, ToolStepResult,
};
use auth::{CredentialKind, CredentialPut, CredentialStore, InMemoryCredentialStore, SecretRef, SecretValue};
use context_engine::compile::{ContextBlock, ContextSource};
use llm_router::credentials::{ProfileId, ProviderProfile};
use llm_router::phase::ReasoningEffort;
use llm_router::provider::{
    CanonicalMessage, CanonicalModelRequest, CanonicalToolSpec, CatalogRevision, ContentPart,
    MessageRole, ModelId, ModelPurpose, ModelRequestId, ModelRef, ModelStream, ModelStreamEvent,
    NormalizedUsage, ProviderCapabilities, ProviderError, ProviderId, ReasoningSupport,
    ToolCall, ToolCallId, ToolName, UsageFieldSet,
};
use llm_router::providers::anthropic::{
    AnthropicAdapter, AnthropicConfig, AnthropicEndpoint,
};
use llm_router::providers::openai_compatible::{
    Http1Transport, HttpTransport, OpenAiApiStyle, OpenAiCompatibleAdapter, OpenAiCompatibleConfig,
    OpenAiCompatibleEndpoint, StaticWireAuth, WireAuthorization,
};
use protocol::TraceContext;

use crate::host::{LiveModelCall, UnconfiguredModel};
use crate::user_config::{
    ActiveModel, ConfigProvider, CredentialSource, DEFAULT_CONTEXT_WINDOW,
    DEFAULT_MAX_OUTPUT_TOKENS, UserConfigError,
};

/// Alias namespace for config-resolved credentials. The value is seeded once
/// at composition; the router resolves by handle only.
const INLINE_ALIAS_PREFIX: &str = "inline:";
const ENV_ALIAS_PREFIX: &str = "env:";
const KEYLESS_ALIAS_PREFIX: &str = "keyless:";
/// Non-empty stand-in so keyless configs pass the resolver's non-empty check;
/// the no-auth transport never puts it on the wire.
const KEYLESS_PLACEHOLDER: &str = "rapidlm-keyless";

/// Monotonic component for request ids (uniqueness within the process).
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Typed adapter-construction failure. Values shown are identifiers from the
/// config, never credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelConfigError {
    Config(UserConfigError),
    ProfileId { profile_id: String, reason: String },
    ModelId { model: String, reason: String },
    BaseUrl { base_url: String, reason: String },
    Capability { reason: String },
    Credential { reason: String },
}

impl fmt::Display for ModelConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(err) => write!(f, "{err}"),
            Self::ProfileId { profile_id, reason } => {
                write!(f, "config model id '{profile_id}' is invalid: {reason}")
            }
            Self::ModelId { model, reason } => {
                write!(f, "config model '{model}' is invalid: {reason}")
            }
            Self::BaseUrl { base_url, reason } => {
                write!(f, "config base_url '{base_url}' is invalid: {reason}")
            }
            Self::Capability { reason } => write!(f, "model capability pin is invalid: {reason}"),
            Self::Credential { reason } => write!(f, "model credential is unusable: {reason}"),
        }
    }
}

impl std::error::Error for ModelConfigError {}

impl From<UserConfigError> for ModelConfigError {
    fn from(err: UserConfigError) -> Self {
        Self::Config(err)
    }
}

/// Wire authorization that sends no bearer token, for keyless local servers.
struct NoWireAuth;

impl WireAuthorization for NoWireAuth {
    fn bearer_token(
        &self,
        _credential: &llm_router::credentials::EphemeralCredential,
        _cancel: &llm_router::provider::CancellationToken,
    ) -> Result<String, ProviderError> {
        Ok(String::new())
    }
}

/// Concrete adapter backend over the boxed blocking transport.
enum Backend<'store> {
    OpenAi(OpenAiCompatibleAdapter<'store, Box<dyn HttpTransport>>),
    Anthropic(AnthropicAdapter<'store, Box<dyn HttpTransport>>),
}

/// A configured provider adapter behind the [`LiveModelCall`] seam.
pub struct ConfiguredModel<'store> {
    backend: Backend<'store>,
    provider: ProviderId,
    model: ModelId,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>,
}

impl<'store> ConfiguredModel<'store> {
    /// Construct the adapter for `active`, seeding `store` with the resolved
    /// credential. No network I/O happens here; request-time constraints
    /// (endpoint shape, bounds) are validated eagerly so exec fails typed.
    pub fn build(
        active: &ActiveModel,
        store: &'store InMemoryCredentialStore,
    ) -> Result<Self, ModelConfigError> {
        let provider = ProviderId::parse(active.entry.provider.as_str()).map_err(|_| {
            ModelConfigError::Capability {
                reason: "provider kind does not map to a router provider id".to_owned(),
            }
        })?;
        let model = ModelId::parse(&active.entry.model).map_err(|_| ModelConfigError::ModelId {
            model: active.entry.model.clone(),
            reason: "the canonical layer allows alphanumerics with single '-', '_', '.', '/', \
                     ':' separators (provider-side ids like vendor/model:tag are carried \
                     verbatim)"
                .to_owned(),
        })?;
        // Validate the profile id first: it names both the router profile and
        // the derived credential alias, so it must satisfy the strictest
        // alphabet of the two (lowercase letters, digits, single dashes).
        ProfileId::parse(&active.profile_id).map_err(|_| ModelConfigError::ProfileId {
            profile_id: active.profile_id.clone(),
            reason: "profile ids are lowercase letters, digits, and single dashes".to_owned(),
        })?;
        let context_limit = active
            .entry
            .context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let max_output = active.entry.max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let capabilities = ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            ReasoningSupport::None,
            true,
            context_limit,
            max_output,
            UsageFieldSet::new(true, false, false, true, false, false, false),
        )
        .map_err(|_| ModelConfigError::Capability {
            reason: "context_window/max_tokens must be positive".to_owned(),
        })?;

        let secret_ref = seed_credential(active, store, &provider)?;
        let profile = ProviderProfile::new(&active.profile_id, provider.clone(), secret_ref)
            .map_err(|_| ModelConfigError::ProfileId {
                profile_id: active.profile_id.clone(),
                reason: "profile ids are lowercase letters, digits, and single dashes".to_owned(),
            })?;

        let transport: Box<dyn HttpTransport> = match &active.credential.plaintext {
            Some(key) => {
                let bearer = StaticWireAuth::bearer(key).map_err(|_| ModelConfigError::Credential {
                    reason: "key contains control characters or exceeds the size bound".to_owned(),
                })?;
                Box::new(Http1Transport::new(bearer))
            }
            None => Box::new(Http1Transport::new(NoWireAuth)),
        };

        let backend = match active.entry.provider {
            ConfigProvider::OpenAiCompatible => {
                let endpoint = OpenAiCompatibleEndpoint::new(
                    &active.entry.base_url,
                    OpenAiApiStyle::ChatCompletions,
                )
                .map_err(|_| ModelConfigError::BaseUrl {
                    base_url: active.entry.base_url.clone(),
                    reason: "expected an http:// or https:// origin without userinfo or \
                             metadata hosts"
                        .to_owned(),
                })?;
                let config = OpenAiCompatibleConfig::new(profile, endpoint, capabilities)
                    .map_err(|_| ModelConfigError::Capability {
                        reason: "the OpenAI-compatible path requires streaming".to_owned(),
                    })?;
                Backend::OpenAi(OpenAiCompatibleAdapter::new(config, transport, store))
            }
            ConfigProvider::Anthropic => {
                let endpoint = AnthropicEndpoint::new(&active.entry.base_url).map_err(|_| {
                    ModelConfigError::BaseUrl {
                        base_url: active.entry.base_url.clone(),
                        reason: "expected an http:// or https:// origin without userinfo or \
                                 metadata hosts"
                            .to_owned(),
                    }
                })?;
                let config = AnthropicConfig::new(profile, endpoint, capabilities)
                    .map_err(|_| ModelConfigError::Capability {
                        reason: "the Anthropic path requires streaming".to_owned(),
                    })?;
                Backend::Anthropic(AnthropicAdapter::new(config, transport, store))
            }
        };

        Ok(Self {
            backend,
            provider,
            model,
            max_output_tokens: active.entry.max_tokens,
            reasoning_effort: active.entry.reasoning_effort,
        })
    }
}

/// Seed the process-local credential store so the router's fail-closed
/// resolver finds an authorized, non-empty record for the profile handle.
fn seed_credential(
    active: &ActiveModel,
    store: &InMemoryCredentialStore,
    provider: &ProviderId,
) -> Result<SecretRef, ModelConfigError> {
    let alias = match &active.credential.source {
        CredentialSource::InlineApiKey => format!("{INLINE_ALIAS_PREFIX}{}", active.profile_id),
        CredentialSource::EnvVar(name) => format!("{ENV_ALIAS_PREFIX}{name}"),
        CredentialSource::Keyless => format!("{KEYLESS_ALIAS_PREFIX}{}", active.profile_id),
    };
    let refer = SecretRef::from_alias(&alias).map_err(|_| ModelConfigError::Credential {
        reason: format!("derived credential alias '{alias}' is invalid"),
    })?;
    let plaintext = active
        .credential
        .plaintext
        .as_deref()
        .unwrap_or(KEYLESS_PLACEHOLDER);
    let value = SecretValue::from_str(refer.clone(), plaintext).map_err(|_| {
        ModelConfigError::Credential {
            reason: "configured key is empty or exceeds the secret size bound".to_owned(),
        }
    })?;
    let put = CredentialPut::ephemeral(value, CredentialKind::ProviderApiKey)
        .with_provider(provider.as_str())
        .map_err(|_| ModelConfigError::Credential {
            reason: "provider id was rejected by the credential store".to_owned(),
        })?;
    let cancel = auth::CancellationToken::new();
    store.put(put, &cancel).map_err(|err| ModelConfigError::Credential {
        reason: format!("credential store rejected the model key: {err:?}"),
    })
}

impl LiveModelCall for ConfiguredModel<'_> {
    fn step(
        &mut self,
        blocks: &[ContextBlock],
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        if cancel.is_cancelled() {
            return Err(ModelStepError::Cancelled);
        }
        let request = build_request(self, blocks, input)?;
        // The agent token is checked on entry and exit; the blocking HTTP
        // call itself is bounded by the transport timeout.
        let router_cancel = llm_router::provider::CancellationToken::new();
        let stream = match &self.backend {
            Backend::OpenAi(adapter) => adapter.invoke_sync(request, &router_cancel),
            Backend::Anthropic(adapter) => adapter.invoke_sync(request, &router_cancel),
        };
        let stream = stream.map_err(map_provider_error)?;
        if cancel.is_cancelled() {
            return Err(ModelStepError::Cancelled);
        }
        fold_stream(&stream)
    }
}

/// Model selection for the exec path: a configured adapter or the typed
/// no-config fallback, behind one [`LiveModelCall`] implementation.
pub enum SelectedModel<'store> {
    Configured(Box<ConfiguredModel<'store>>),
    Unconfigured(UnconfiguredModel),
}

impl LiveModelCall for SelectedModel<'_> {
    fn step(
        &mut self,
        blocks: &[ContextBlock],
        input: &ModelStepInput<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        match self {
            Self::Configured(model) => model.step(blocks, input, cancel),
            Self::Unconfigured(fallback) => fallback.step(blocks, input, cancel),
        }
    }
}

/// Map canonical request-construction failures onto the step error surface.
/// Bounds failures keep their meaning so the context-recovery loop can react.
fn build_request(
    model: &ConfiguredModel<'_>,
    blocks: &[ContextBlock],
    input: &ModelStepInput<'_>,
) -> Result<CanonicalModelRequest, ModelStepError> {
    let mut messages = Vec::with_capacity(
        blocks.len() + input.history().len() * 2 + 1,
    );
    for block in blocks {
        let role = if matches!(block.source(), ContextSource::System) {
            MessageRole::System
        } else {
            MessageRole::User
        };
        let part = ContentPart::text(block.text()).map_err(|_| ModelStepError::BoundExceeded)?;
        messages.push(
            CanonicalMessage::new(role, vec![part], None, Vec::new())
                .map_err(|_| ModelStepError::BoundExceeded)?,
        );
    }
    // Per-call tool-result channel: replay every completed exchange of the
    // turn — the assistant tool-call message followed by one tool-role
    // message per executed call, each bearing its id and text outcome. The
    // history is the model's working memory; dropping older exchanges makes
    // multi-step tasks impossible, so only the byte budget prunes, and it
    // prunes whole oldest exchanges. This replaces the single flat
    // "tool results:" user report.
    let history = kept_history(input.history());
    for exchange in history {
        if !exchange.calls().is_empty() {
            let tool_calls = exchange
                .calls()
                .iter()
                .map(|call| {
                    Ok(ToolCall::new(
                        ToolCallId::parse(call.call_id()).map_err(map_provider_error)?,
                        ToolName::parse(call.tool()).map_err(map_provider_error)?,
                        call.arguments(),
                    )
                    .map_err(map_provider_error)?)
                })
                .collect::<Result<Vec<_>, ModelStepError>>()?;
            messages.push(
                CanonicalMessage::new(MessageRole::Assistant, Vec::new(), None, tool_calls)
                    .map_err(|_| ModelStepError::BoundExceeded)?,
            );
        }
        for result in exchange.results() {
            let (raw_call_id, text) = tool_result_text(result);
            let call_id = ToolCallId::parse(raw_call_id).map_err(map_provider_error)?;
            // Inline vision content: `DATA_URL:<data:image/...>` lines become
            // image parts so vision-capable models see the actual pixels.
            let mut parts: Vec<ContentPart> = Vec::new();
            let mut plain = String::new();
            for line in text.lines() {
                if let Some(data_url) = line.strip_prefix("DATA_URL:") {
                    if !plain.is_empty() {
                        plain.push('\n');
                    }
                    match ContentPart::image_data(data_url.to_owned()) {
                        Ok(part) => {
                            parts.push(part);
                            plain.clear();
                        }
                        Err(_) => {
                            plain.push_str(line);
                            plain.push('\n');
                        }
                    }
                } else {
                    plain.push_str(line);
                    plain.push('\n');
                }
            }
            let plain = plain.trim_end().to_owned();
            if !plain.is_empty() {
                parts.push(
                    ContentPart::text(plain).map_err(|_| ModelStepError::BoundExceeded)?,
                );
            }
            if !parts.is_empty() {
                messages.push(
                    CanonicalMessage::new(
                        MessageRole::Tool,
                        parts,
                        Some(call_id),
                        Vec::new(),
                    )
                    .map_err(|_| ModelStepError::BoundExceeded)?,
                );
            }
        }
    }


    let request_id = next_request_id()?;
    let model_ref = ModelRef::new(model.provider.clone(), model.model.clone());
    let catalog_revision =
        CatalogRevision::new(1).map_err(|_| ModelStepError::Failed)?;
    let router_cancel = llm_router::provider::CancellationToken::new();
    // The driver's tool surface becomes structured provider tool schemas, so
    // tool-capable models can propose calls through the tool-call channel
    // instead of prose. Failures here are typed provider failures.
    let mut tools = Vec::with_capacity(input.tool_surface().len());
    for spec in input.tool_surface() {
        let name = ToolName::parse(spec.name()).map_err(map_provider_error)?;
        tools.push(
            CanonicalToolSpec::new(
                name,
                spec.description(),
                spec.parameters().clone(),
            )
            .map_err(map_provider_error)?,
        );
    }
    let request = CanonicalModelRequest::new(
        request_id,
        model_ref,
        ModelPurpose::Chat,
        messages,
        tools,
        model.max_output_tokens,
        catalog_revision,
        TraceContext::root(),
        &router_cancel,
    )
    .map_err(map_provider_error)?;
    // The Anthropic adapter does not send thinking budgets yet; a configured
    // effort there is validated and carried but inert (documented at
    // llm_router::phase).
    Ok(match model.reasoning_effort {
        Some(effort) => request.with_reasoning_effort(effort),
        None => request,
    })
}

fn next_request_id() -> Result<ModelRequestId, ModelStepError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let seq = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    ModelRequestId::parse(format!("live-{nanos:x}-{seq}")).map_err(|_| ModelStepError::Failed)
}

/// Hard bounds for the replayed tool-exchange history: the newest exchange
/// is always kept, older ones are dropped wholesale once a bound is hit.
const MAX_TOOL_HISTORY_EXCHANGES: usize = 12;
const MAX_TOOL_HISTORY_BYTES: usize = 24 * 1024;

fn exchange_wire_bytes(exchange: &agent_runtime::ToolStepExchange) -> usize {
    let calls: usize = exchange
        .calls()
        .iter()
        .map(|call| call.call_id().len() + call.tool().len() + call.arguments().len())
        .sum();
    let results: usize = exchange
        .results()
        .iter()
        .map(|result| match result {
            ToolStepResult::Succeeded { summary, .. } => summary.len(),
            ToolStepResult::Failed { detail, .. } | ToolStepResult::Denied { detail, .. } => {
                detail.as_deref().map_or(0, str::len)
            }
            ToolStepResult::ApprovalRequired { .. } => 0,
        })
        .sum();
    calls + results + 128
}

fn kept_history(
    history: &[agent_runtime::ToolStepExchange],
) -> &[agent_runtime::ToolStepExchange] {
    let mut start = history.len();
    let mut kept = 0usize;
    let mut budget = MAX_TOOL_HISTORY_BYTES;
    while start > 0 {
        let size = exchange_wire_bytes(&history[start - 1]);
        if kept >= MAX_TOOL_HISTORY_EXCHANGES || (kept > 0 && size > budget) {
            break;
        }
        start -= 1;
        kept += 1;
        budget = budget.saturating_sub(size);
    }
    &history[start..]
}

/// Model-visible text for one tool outcome. This is the per-call tool-result
/// content the next request carries; a denial keeps its typed reason so a
/// headless refusal is never a silent pass.
fn tool_result_text(tool: &ToolStepResult) -> (&str, String) {
    match tool {
        ToolStepResult::Succeeded { call_id, summary } => (call_id, summary.clone()),
        ToolStepResult::Failed {
            call_id,
            handled,
            detail,
        } => (
            call_id,
            match (handled, detail.as_deref()) {
                (true, Some(detail)) => format!("failed: {detail}"),
                (true, None) => "failed (the model can correct and retry)".to_owned(),
                (false, _) => "failed (unhandled; the turn will stop)".to_owned(),
            },
        ),
        ToolStepResult::Denied { call_id, detail } => (
            call_id,
            match detail.as_deref() {
                Some(detail) => format!("denied: {detail}"),
                None => "denied by permission policy".to_owned(),
            },
        ),
        ToolStepResult::ApprovalRequired { call_id } => (
            call_id,
            "approval required: the user must approve this call".to_owned(),
        ),
    }
}

/// Typed provider → step error mapping. `ContextTooLarge` keeps its meaning
/// (`BoundExceeded`) so the host's context-recovery loop compacts and retries.
/// Every other provider failure keeps its operator-actionable cause class;
/// nothing collapses into an unspecified `Failed`.
fn map_provider_error(err: ProviderError) -> ModelStepError {
    match err {
        ProviderError::Cancelled => ModelStepError::Cancelled,
        ProviderError::ContextTooLarge | ProviderError::BoundExceeded => {
            ModelStepError::BoundExceeded
        }
        ProviderError::AuthFailed => ModelStepError::ProviderFailed {
            cause: FailureCause::Auth,
        },
        ProviderError::Connection => ModelStepError::ProviderFailed {
            cause: FailureCause::Connection,
        },
        ProviderError::RateLimited { retry_after_ms } => ModelStepError::ProviderFailed {
            cause: FailureCause::Transient { retry_after_ms },
        },
        ProviderError::Transient => ModelStepError::ProviderFailed {
            cause: FailureCause::Transient { retry_after_ms: None },
        },
        ProviderError::InvalidRequest | ProviderError::Permanent | ProviderError::UnknownVariant => {
            ModelStepError::ProviderFailed {
                cause: FailureCause::Rejected,
            }
        }
    }
}

/// Fold the collected provider stream into one step output. Text deltas form
/// the terminal text; tool-call deltas form proposed calls (the tool driver
/// decides validity downstream).
fn fold_stream(stream: &ModelStream) -> Result<ModelStepOutput, ModelStepError> {
    let mut text = String::new();
    // call_id -> (tool name, accumulated arguments)
    let mut tools: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut usage: Option<&NormalizedUsage> = None;
    for event in stream.events() {
        match event {
            ModelStreamEvent::TextDelta { text: delta } => text.push_str(delta),
            ModelStreamEvent::ToolCallStart { call_id, name } => {
                tools.insert(call_id.as_str().to_owned(), (name.as_str().to_owned(), String::new()));
            }
            ModelStreamEvent::ToolCallArgumentsDelta {
                call_id,
                arguments_delta,
            } => {
                if let Some(entry) = tools.get_mut(call_id.as_str()) {
                    entry.1.push_str(arguments_delta);
                }
            }
            ModelStreamEvent::Usage(normalized) => usage = Some(normalized),
            ModelStreamEvent::Completed { usage: normalized, .. } => usage = Some(normalized),
            ModelStreamEvent::Failed { error } => return Err(map_provider_error(error.clone())),
        }
    }
    let tokens = usage.map(usage_total_tokens).unwrap_or(0);
    if tools.is_empty() {
        return Ok(ModelStepOutput::Terminal { text, tokens });
    }
    let calls = tools
        .into_iter()
        .map(|(call_id, (tool, arguments))| {
            ProposedToolCall::new(call_id, tool, arguments).map_err(|_| ModelStepError::Failed)
        })
        .collect::<Result<Vec<_>, ModelStepError>>()?;
    Ok(ModelStepOutput::ToolCalls { calls, tokens })
}

fn usage_total_tokens(usage: &NormalizedUsage) -> u64 {
    usage
        .input_tokens()
        .unwrap_or(0)
        .saturating_add(usage.output_tokens().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::PreservedLiveContext;
    use crate::user_config::{
        CredentialSource, ModelEntry, ResolvedCredential,
    };
    use llm_router::provider::{FinishReason, UsageCost};

    fn entry(model: &str, base_url: &str) -> ModelEntry {
        ModelEntry {
            provider: ConfigProvider::OpenAiCompatible,
            model: model.to_owned(),
            base_url: base_url.to_owned(),
            name: None,
            api_key: Some("test-key".to_owned()),
            env_key: Vec::new(),
            max_tokens: None,
            context_window: None,
            reasoning_effort: None,
        }
    }

    fn active(model: &str, base_url: &str, profile_id: &str) -> ActiveModel {
        // Some tests deliberately pass invalid profile ids; the route only
        // needs a syntactically valid placeholder for those to reach build().
        let route_profile = llm_router::ProfileId::parse(profile_id)
            .unwrap_or_else(|_| llm_router::ProfileId::parse("placeholder").expect("id"));
        ActiveModel {
            profile_id: profile_id.to_owned(),
            entry: entry(model, base_url),
            credential: ResolvedCredential {
                plaintext: Some("test-key".to_owned()),
                source: CredentialSource::InlineApiKey,
            },
            phase_route: llm_router::PhaseRoute::new(route_profile),
        }
    }

    fn stream(events: Vec<ModelStreamEvent>) -> ModelStream {
        let request_id = ModelRequestId::parse("live-test").expect("id");
        let model = ModelRef::new(
            ProviderId::parse("openai-compatible").expect("provider"),
            ModelId::parse("test-model").expect("model"),
        );
        let cancel = llm_router::provider::CancellationToken::new();
        ModelStream::from_events(request_id, model, events, &cancel).expect("stream")
    }

    #[test]
    fn build_rejects_model_ids_outside_the_canonical_alphabet() {
        let store = InMemoryCredentialStore::new();
        let model_local = active("gpt 4", "http://127.0.0.1:11434/v1", "local");
        let err = match ConfiguredModel::build(&model_local, &store) {
            Err(err) => err,
            Ok(_) => panic!("expected ModelId rejection"),
        };
        // The rendered message never contains the credential.
        let rendered = format!("{err}");
        match err {
            ModelConfigError::ModelId { model, reason } => {
                assert_eq!(model, "gpt 4");
                assert!(reason.contains("separators"), "hint missing: {reason}");
            }
            other => panic!("expected ModelId error, got {other:?}"),
        }
        assert!(!rendered.contains("test-key"));
    }

    #[test]
    fn build_accepts_provider_style_model_ids_and_https_origins() {
        let store = InMemoryCredentialStore::new();
        // OpenRouter-style vendor/tag id over a TLS origin.
        let remote = active(
            "inclusionai/ling-3.0-flash-fin:free",
            "https://openrouter.ai/api/v1",
            "openrouter",
        );
        ConfiguredModel::build(&remote, &store).expect("remote build");
    }

    #[test]
    fn build_rejects_profile_ids_outside_the_router_alphabet() {
        let store = InMemoryCredentialStore::new();
        let model_bad_profile = active("test-model", "http://127.0.0.1:11434/v1", "Local Host");
        let err = match ConfiguredModel::build(&model_bad_profile, &store) {
            Err(err) => err,
            Ok(_) => panic!("expected ProfileId rejection"),
        };
        assert!(matches!(err, ModelConfigError::ProfileId { .. }));
    }

    #[test]
    fn build_seeds_keyless_and_inline_credentials() {
        // Inline key.
        let store = InMemoryCredentialStore::new();
        let model_inline = active("test-model", "http://127.0.0.1:1", "local");
        ConfiguredModel::build(&model_inline, &store).expect("inline build");
        let refer = SecretRef::from_alias("inline:local").expect("alias");
        let resolved = store
            .get(&refer, &auth::CancellationToken::new())
            .expect("seeded");
        // The store holds the key so the resolver's non-empty check passes.
        let debug = format!("{resolved:?}");
        assert!(!debug.contains("test-key"), "secret leaked via Debug");

        // Keyless: seeded placeholder, build still succeeds.
        let mut keyless = active("test-model", "http://127.0.0.1:1", "keyless-local");
        keyless.credential = ResolvedCredential {
            plaintext: None,
            source: CredentialSource::Keyless,
        };
        let store = InMemoryCredentialStore::new();
        ConfiguredModel::build(&keyless, &store).expect("keyless build");
        let refer = SecretRef::from_alias("keyless:keyless-local").expect("alias");
        store
            .get(&refer, &auth::CancellationToken::new())
            .expect("seeded placeholder");
    }

    #[test]
    fn provider_error_mapping_keeps_bounds_and_cancellation_typed() {
        assert_eq!(
            map_provider_error(ProviderError::ContextTooLarge),
            ModelStepError::BoundExceeded
        );
        assert_eq!(
            map_provider_error(ProviderError::BoundExceeded),
            ModelStepError::BoundExceeded
        );
        assert_eq!(
            map_provider_error(ProviderError::Cancelled),
            ModelStepError::Cancelled
        );
    }

    #[test]
    fn provider_error_mapping_keeps_distinct_cause_classes() {
        // Auth vs connection vs rejection vs transient must stay distinguishable:
        // the CLI formats each into its own operator-actionable message.
        assert_eq!(
            map_provider_error(ProviderError::AuthFailed),
            ModelStepError::ProviderFailed {
                cause: FailureCause::Auth
            }
        );
        assert_eq!(
            map_provider_error(ProviderError::Connection),
            ModelStepError::ProviderFailed {
                cause: FailureCause::Connection
            }
        );
        assert_eq!(
            map_provider_error(ProviderError::RateLimited {
                retry_after_ms: Some(900)
            }),
            ModelStepError::ProviderFailed {
                cause: FailureCause::Transient {
                    retry_after_ms: Some(900)
                }
            }
        );
        assert_eq!(
            map_provider_error(ProviderError::Transient),
            ModelStepError::ProviderFailed {
                cause: FailureCause::Transient { retry_after_ms: None }
            }
        );
        for rejected in [
            ProviderError::InvalidRequest,
            ProviderError::Permanent,
            ProviderError::UnknownVariant,
        ] {
            assert_eq!(
                map_provider_error(rejected),
                ModelStepError::ProviderFailed {
                    cause: FailureCause::Rejected
                }
            );
        }
    }

    #[test]
    fn fold_stream_collects_text_and_usage_tokens() {
        let usage = NormalizedUsage::new(
            Some(11),
            None,
            None,
            Some(7),
            None,
            None,
            UsageCost::Unknown,
        );
        let stream = stream(vec![
            ModelStreamEvent::TextDelta {
                text: "hel".to_owned(),
            },
            ModelStreamEvent::TextDelta {
                text: "lo".to_owned(),
            },
            ModelStreamEvent::Usage(usage.clone()),
            ModelStreamEvent::Completed {
                finish: FinishReason::Stop,
                usage,
            },
        ]);
        let output = fold_stream(&stream).expect("fold");
        match output {
            ModelStepOutput::Terminal { text, tokens } => {
                assert_eq!(text, "hello");
                assert_eq!(tokens, 18);
            }
            other => panic!("expected terminal, got {other:?}"),
        }
    }

    #[test]
    fn fold_stream_collects_tool_calls_with_arguments() {
        let call_id = llm_router::provider::ToolCallId::parse("call-1").expect("call id");
        let name = llm_router::provider::ToolName::parse("read-file").expect("name");
        let usage = NormalizedUsage::new(None, None, None, None, None, None, UsageCost::Unknown);
        let stream = stream(vec![
            ModelStreamEvent::ToolCallStart {
                call_id: call_id.clone(),
                name: name.clone(),
            },
            ModelStreamEvent::ToolCallArgumentsDelta {
                call_id,
                arguments_delta: "{\"path\":\"a.rs\"}".to_owned(),
            },
            ModelStreamEvent::Completed {
                finish: FinishReason::ToolCalls,
                usage,
            },
        ]);
        let output = fold_stream(&stream).expect("fold");
        match output {
            ModelStepOutput::ToolCalls { calls, tokens } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].call_id(), "call-1");
                assert_eq!(calls[0].tool(), "read-file");
                assert_eq!(calls[0].arguments(), "{\"path\":\"a.rs\"}");
                assert_eq!(tokens, 0);
            }
            other => panic!("expected tool calls, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_token_fails_before_any_provider_call() {
        let store = InMemoryCredentialStore::new();
        let active = active("test-model", "http://127.0.0.1:1", "local");
        let mut configured = ConfiguredModel::build(&active, &store).expect("build");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let packet_blocks: Vec<ContextBlock> = Vec::new();
        let err = configured
            .step(&packet_blocks, &ModelStepInput::without_tools(1), &cancel)
            .expect_err("cancelled");
        assert_eq!(err, ModelStepError::Cancelled);
    }

    #[test]
    fn selected_model_delegates_to_the_typed_fallback() {
        let mut selected = SelectedModel::Unconfigured(UnconfiguredModel);
        let cancel = CancellationToken::new();
        let blocks: Vec<ContextBlock> = Vec::new();
        assert_eq!(
            selected
                .step(&blocks, &ModelStepInput::without_tools(1), &cancel)
                .expect_err("fallback"),
            ModelStepError::Failed
        );
    }

    #[test]
    fn preserved_context_blocks_map_into_a_request() {
        // The preserved live context compiles into blocks; the bridge must
        // accept them (bounds hold) — verified through the public host path.
        let preserved = PreservedLiveContext::new(
            "ship the feature",
            vec!["tests pass".to_owned()],
            "",
            "",
            1024,
            64,
        )
        .expect("preserved");
        let packet = crate::host::build_packet(&preserved, None).expect("packet");
        let blocks = packet.blocks();
        assert!(!blocks.is_empty());
        // Direct request construction from those blocks (no adapter call).
        let store = InMemoryCredentialStore::new();
        let active = active("test-model", "http://127.0.0.1:1", "local");
        let configured = ConfiguredModel::build(&active, &store).expect("build");
        let built =
            build_request(&configured, blocks, &ModelStepInput::without_tools(1)).expect("request");
        assert!(!built.messages().is_empty());
        assert!(built.tools().is_empty());
    }

    #[test]
    fn tool_surface_is_advertised_as_provider_tool_schemas() {
        let store = InMemoryCredentialStore::new();
        let active = active("test-model", "http://127.0.0.1:1", "local");
        let configured = ConfiguredModel::build(&active, &store).expect("build");
        let surface = vec![agent_runtime::ToolSurface::new(
            "workspace_write",
            "create a file",
            serde_json::json!({"type": "object", "required": ["path", "content"]}),
        )];
        let input = ModelStepInput::with_history(1, &[], &surface);
        let built = build_request(&configured, &[], &input).expect("request");
        assert_eq!(built.tools().len(), 1);
        assert_eq!(built.tools()[0].name().as_str(), "workspace_write");
        assert_eq!(built.tools()[0].description(), "create a file");
        assert!(built.tools()[0].parameters().is_object());
    }

    #[test]
    fn tool_results_become_one_tool_role_message_per_call() {
        // Two executed calls in the prior step: the next request must carry
        // the assistant tool-call message followed by one tool-role message
        // per call, each with its own call id and text outcome — and no flat
        // "tool results:" user report.
        let store = InMemoryCredentialStore::new();
        let active = active("test-model", "http://127.0.0.1:1", "local");
        let configured = ConfiguredModel::build(&active, &store).expect("build");
        let pending = vec![
            ProposedToolCall::new("c1", "repo_read", r#"{"path":"a.txt"}"#).expect("c1"),
            ProposedToolCall::new(
                "c2",
                "workspace_patch",
                r#"{"path":"b.txt","old":"x","new":"y"}"#,
            )
            .expect("c2"),
        ];
        let prior = vec![
            ToolStepResult::Succeeded {
                call_id: "c1".to_owned(),
                summary: "file body".to_owned(),
            },
            ToolStepResult::Denied {
                call_id: "c2".to_owned(),
                detail: Some("workspace_patch denied: denied by an explicit deny rule".to_owned()),
            },
        ];
        let exchange = agent_runtime::ToolStepExchange::new(pending, prior);
        let history = vec![exchange];
        let input = ModelStepInput::with_history(2, &history, &[]);
        let built = build_request(&configured, &[], &input).expect("request");
        let messages = built.messages();
        // No flat user report.
        for message in messages {
            for part in message.parts() {
                assert!(
                    !part_text(part).contains("tool results:"),
                    "flat report must be gone"
                );
            }
        }
        // Assistant tool-call message echoes the proposals in order.
        let assistant = messages
            .iter()
            .find(|message| message.role() == MessageRole::Assistant)
            .expect("assistant tool-call message");
        assert_eq!(assistant.tool_calls().len(), 2);
        assert_eq!(assistant.tool_calls()[0].call_id().as_str(), "c1");
        assert_eq!(assistant.tool_calls()[0].name().as_str(), "repo_read");
        assert_eq!(assistant.tool_calls()[1].call_id().as_str(), "c2");
        // One tool-role message per call, in call order, with per-call text.
        let tool_messages: Vec<&CanonicalMessage> = messages
            .iter()
            .filter(|message| message.role() == MessageRole::Tool)
            .collect();
        assert_eq!(tool_messages.len(), 2);
        assert_eq!(tool_messages[0].tool_call_id().map(|id| id.as_str()), Some("c1"));
        assert_eq!(part_text(&tool_messages[0].parts()[0]), "file body");
        assert_eq!(tool_messages[1].tool_call_id().map(|id| id.as_str()), Some("c2"));
        let denied_text = part_text(&tool_messages[1].parts()[0]);
        assert!(denied_text.starts_with("denied: "), "{denied_text}");
        assert!(denied_text.contains("deny rule"), "{denied_text}");

        // A step with no prior tool results adds neither assistant nor tool
        // messages.
        let built = build_request(&configured, &[], &ModelStepInput::without_tools(1))
            .expect("request");
        assert!(built
            .messages()
            .iter()
            .all(|message| message.role() != MessageRole::Tool));
        assert!(built
            .messages()
            .iter()
            .all(|message| message.role() != MessageRole::Assistant));
    }

    fn part_text(part: &ContentPart) -> &str {
        match part {
            ContentPart::Text { text } => text,
            ContentPart::Image { .. } | ContentPart::ImageData { .. } => "",
        }
    }
}
