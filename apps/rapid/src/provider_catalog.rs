//! Bounded, inert model-directory snapshots. Import, credentials and network
//! access are separate explicit operations; catalog data grants no authority.
pub mod cache;
pub mod cli;
pub mod import;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PROVIDERS: usize = 1024;
pub const MAX_MODELS_PER_PROVIDER: usize = 8192;
pub const MAX_MODELS_TOTAL: usize = 40_000;
pub const DEFAULT_SOURCE: &str = "https://models.dev/api.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogError {
    TooLarge,
    InvalidDocument,
    TooManyProviders,
    TooManyModels,
    TooManySources,
    InvalidSource,
    Network,
    Cancelled,
    HttpStatus(u16),
    Storage,
    CorruptCache,
    Busy,
    NoCache,
    InvalidPolicy,
}
impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider catalog: {self:?}")
    }
}
impl std::error::Error for CatalogError {}

/// Fetch exactly one explicitly chosen source, without credentials or redirect
/// following, through the same TLS, proxy, address guards and dial gates used
/// by model connections. The caller decides whether and where to cache it.
pub fn fetch(
    source: &str,
    proxy: Option<llm_router::providers::dial::ProxyConfig>,
    gate: Option<std::sync::Arc<dyn llm_router::providers::dial::DialGate>>,
    cancel: &llm_router::CancellationToken,
) -> Result<Vec<u8>, CatalogError> {
    use llm_router::providers::openai_compatible::{Http1Transport, StaticWireAuth};
    crate::provider_accounts::endpoint(source).map_err(|_| CatalogError::InvalidSource)?;
    let exact = crate::setup::endpoint_egress(source, proxy.as_ref())
        .map_err(|_| CatalogError::InvalidSource)?;
    let mut transport = Http1Transport::with_limits(
        StaticWireAuth::bearer("catalog-public").map_err(|_| CatalogError::InvalidSource)?,
        std::time::Duration::from_secs(10),
        MAX_CATALOG_BYTES,
    )
    .with_dial_gate(std::sync::Arc::new(CatalogGate { extra: gate, exact }));
    if let Some(proxy) = proxy {
        transport = transport.with_proxy(proxy);
    }
    let headers = vec![
        ("Accept".to_owned(), "application/json".to_owned()),
        (
            "User-Agent".to_owned(),
            format!("RapidLM/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    let response = transport
        .get_raw(source, &headers, cancel)
        .map_err(|error| match error {
            llm_router::ProviderError::Cancelled => CatalogError::Cancelled,
            llm_router::ProviderError::BoundExceeded => CatalogError::TooLarge,
            _ => CatalogError::Network,
        })?;
    if response.status != 200 {
        return Err(CatalogError::HttpStatus(response.status));
    }
    Ok(response.body)
}
struct CatalogGate {
    extra: Option<std::sync::Arc<dyn llm_router::providers::dial::DialGate>>,
    exact: crate::provider_egress::ProviderEgress,
}
impl llm_router::providers::dial::DialGate for CatalogGate {
    fn permit(
        &self,
        target: llm_router::providers::dial::DialTarget<'_>,
        via: Option<&llm_router::providers::dial::ProxyTarget>,
        addresses: &[std::net::SocketAddr],
    ) -> Result<Vec<std::net::SocketAddr>, llm_router::ProviderError> {
        let allowed = match &self.extra {
            Some(extra) => extra.permit(target, via, addresses)?,
            None => addresses.to_vec(),
        };
        self.exact.permit(target, via, &allowed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Provenance {
    pub source: String,
    pub sha256: String,
    pub fetched_at: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Registry {
    pub provenance: Provenance,
    pub providers: BTreeMap<String, Provider>,
    pub skipped_providers: usize,
    pub skipped_models: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    OpenAi,
    OpenAiResponses,
    Anthropic,
    GoogleGenAi,
    VertexGenAi,
    Bedrock,
    AzureChat,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WireChoice {
    Declared { wire: Wire },
    GuessedOpenAi,
    Unsupported,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", content = "url", rename_all = "snake_case")]
pub enum Endpoint {
    Present(String),
    Missing,
    Invalid,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Provider {
    pub id: String,
    pub name: Option<String>,
    pub wire: WireChoice,
    pub endpoint: Endpoint,
    pub env_hints: Vec<String>,
    pub models: BTreeMap<String, Model>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Model {
    pub id: String,
    pub name: Option<String>,
    pub context: u32,
    pub max_input: Option<u32>,
    pub max_output: Option<u32>,
    pub tools: Option<bool>,
    pub reasoning: Option<bool>,
    pub reasoning_efforts: Vec<String>,
    pub reasoning_off_effort: Option<String>,
    pub reasoning_toggle: bool,
    pub always_thinking: bool,
    pub reasoning_field: Option<String>,
    pub dynamically_loaded_tools: bool,
    pub image_input: bool,
    pub video_input: bool,
    pub audio_input: bool,
    pub lifecycle: Lifecycle,
    pub wire_override: Option<WireChoice>,
    pub endpoint_override: Option<Endpoint>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Active,
    Alpha,
    Deprecated,
    Other,
}

/// Parse a snapshot obtained by an explicit caller. Its hash is over the exact
/// source bytes; it is provenance, not an authenticity signature.
pub fn parse(source: &str, bytes: &[u8], fetched_at: u64) -> Result<Registry, CatalogError> {
    crate::provider_accounts::endpoint(source).map_err(|_| CatalogError::InvalidSource)?;
    let source = crate::mcp_oauth::Url::parse(source)
        .map_err(|_| CatalogError::InvalidSource)?
        .to_string();
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CatalogError::TooLarge);
    }
    let data: Value = serde_json::from_slice(bytes).map_err(|_| CatalogError::InvalidDocument)?;
    let root = data.as_object().ok_or(CatalogError::InvalidDocument)?;
    if root.len() > MAX_PROVIDERS {
        return Err(CatalogError::TooManyProviders);
    }
    let mut providers = BTreeMap::new();
    let (mut skipped_providers, mut skipped_models, mut total) = (0, 0, 0usize);
    for (key, raw) in root {
        let Some(object) = raw.as_object() else {
            skipped_providers += 1;
            continue;
        };
        if llm_router::ProviderId::parse(key).is_err() {
            skipped_providers += 1;
            continue;
        }
        if object
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id != key)
        {
            skipped_providers += 1;
            continue;
        }
        let Some(models) = object.get("models").and_then(Value::as_object) else {
            skipped_providers += 1;
            continue;
        };
        if models.len() > MAX_MODELS_PER_PROVIDER {
            return Err(CatalogError::TooManyModels);
        }
        total = total
            .checked_add(models.len())
            .ok_or(CatalogError::TooManyModels)?;
        if total > MAX_MODELS_TOTAL {
            return Err(CatalogError::TooManyModels);
        }
        let mut normalized = BTreeMap::new();
        for value in models.values() {
            let Some(model) = parse_model(value) else {
                skipped_models += 1;
                continue;
            };
            if normalized.contains_key(&model.id) {
                return Err(CatalogError::InvalidDocument);
            }
            normalized.insert(model.id.clone(), model);
        }
        let hints = object
            .get("env")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|name| valid_env_name(name))
                    .take(16)
                    .map(str::to_owned)
                    .collect()
            });
        providers.insert(
            key.clone(),
            Provider {
                id: key.clone(),
                name: label(object.get("name")),
                wire: wire_choice(object.get("type"), object.get("npm")),
                endpoint: endpoint(object.get("api")),
                env_hints: hints,
                models: normalized,
            },
        );
    }
    Ok(Registry {
        provenance: Provenance {
            source,
            sha256: format!("{:x}", Sha256::digest(bytes)),
            fetched_at,
        },
        providers,
        skipped_providers,
        skipped_models,
    })
}
fn label(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?;
    (!value.is_empty()
        && value.len() <= 256
        && value.chars().all(|character| {
            !character.is_control() && !protocol::text::changes_the_line(character)
        }))
    .then(|| value.to_owned())
}
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
}
fn endpoint(value: Option<&Value>) -> Endpoint {
    match value {
        None | Some(Value::Null) => Endpoint::Missing,
        Some(Value::String(raw)) if !raw.contains("${") => {
            crate::provider_accounts::endpoint(raw).map_or(Endpoint::Invalid, Endpoint::Present)
        }
        _ => Endpoint::Invalid,
    }
}
fn wire_choice(typ: Option<&Value>, npm: Option<&Value>) -> WireChoice {
    let declared = match typ {
        Some(Value::String(s)) if !s.is_empty() => Some(match s.as_str() {
            "openai" | "openai-compatible" => Wire::OpenAi,
            "openai_responses" | "openai-responses" => Wire::OpenAiResponses,
            "anthropic" => Wire::Anthropic,
            "google-genai" => Wire::GoogleGenAi,
            "vertexai" => Wire::VertexGenAi,
            "bedrock" => Wire::Bedrock,
            "azure" => Wire::AzureChat,
            _ => return WireChoice::Unsupported,
        }),
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::Null) | None => None,
        _ => return WireChoice::Unsupported,
    };
    if let Some(wire) = declared {
        return WireChoice::Declared { wire };
    }
    match npm.and_then(Value::as_str) {
        Some("@ai-sdk/openai" | "@ai-sdk/openai-compatible") => {
            WireChoice::Declared { wire: Wire::OpenAi }
        }
        Some("@ai-sdk/anthropic") => WireChoice::Declared {
            wire: Wire::Anthropic,
        },
        Some("@ai-sdk/google") => WireChoice::Declared {
            wire: Wire::GoogleGenAi,
        },
        Some("@ai-sdk/google-vertex") => WireChoice::Declared {
            wire: Wire::VertexGenAi,
        },
        Some("@ai-sdk/amazon-bedrock") => WireChoice::Declared {
            wire: Wire::Bedrock,
        },
        Some("@ai-sdk/azure") => WireChoice::Declared {
            wire: Wire::AzureChat,
        },
        Some(name) if name.to_ascii_lowercase().contains("cohere") => WireChoice::Unsupported,
        Some(_) | None if npm.is_none_or(Value::is_string) || npm == Some(&Value::Null) => {
            WireChoice::GuessedOpenAi
        }
        _ => WireChoice::Unsupported,
    }
}

fn parse_model(value: &Value) -> Option<Model> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?;
    llm_router::ModelId::parse(id).ok()?;
    if [
        Some(id),
        object.get("name").and_then(Value::as_str),
        object.get("family").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .any(|text| {
        let lower = text.to_ascii_lowercase();
        lower.contains("embedding") || lower.split(['-', '_', '/']).any(|part| part == "embed")
    }) {
        return None;
    }
    let limit = object.get("limit")?.as_object()?;
    let context = limit
        .get("context")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)?;
    let limit_number = |key: &str| {
        limit
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0 && *n <= context)
    };
    let modalities = object.get("modalities").and_then(Value::as_object);
    if modalities
        .and_then(|m| m.get("output"))
        .and_then(Value::as_array)
        .is_some_and(|m| !m.iter().any(|v| v.as_str() == Some("text")))
    {
        return None;
    }
    let has_input = |kind| {
        modalities
            .and_then(|m| m.get("input"))
            .and_then(Value::as_array)
            .is_some_and(|inputs| inputs.iter().any(|v| v.as_str() == Some(kind)))
    };
    let life = match object.get("status").and_then(Value::as_str) {
        None => Lifecycle::Active,
        Some("alpha") => Lifecycle::Alpha,
        Some("deprecated") => Lifecycle::Deprecated,
        Some("beta") => Lifecycle::Active,
        Some(_) => Lifecycle::Other,
    };
    let override_object = object.get("provider").and_then(Value::as_object);
    let mut reasoning_efforts = Vec::new();
    let mut reasoning_off_effort = None;
    let mut reasoning_toggle = false;
    if let Some(options) = object.get("reasoning_options").and_then(Value::as_array) {
        if options.len() > 32 {
            return None;
        }
        for option in options {
            match option.get("type").and_then(Value::as_str) {
                Some("toggle") => reasoning_toggle = true,
                Some("effort") => {
                    let Some(values) = option.get("values").and_then(Value::as_array) else {
                        continue;
                    };
                    if values.len() > 32 {
                        return None;
                    }
                    for value in values {
                        if value.is_null() {
                            reasoning_off_effort.get_or_insert_with(|| "none".to_owned());
                            continue;
                        }
                        let Some(name) = value.as_str().filter(|name| {
                            !name.is_empty()
                                && name.len() <= 32
                                && name.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
                                })
                        }) else {
                            continue;
                        };
                        if name.eq_ignore_ascii_case("none") {
                            reasoning_off_effort = Some(name.to_owned());
                        } else if !reasoning_efforts.iter().any(|existing| existing == name) {
                            reasoning_efforts.push(name.to_owned());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let always_thinking =
        !reasoning_efforts.is_empty() && reasoning_off_effort.is_none() && !reasoning_toggle;
    let reasoning_field = object
        .get("interleaved")
        .and_then(Value::as_object)
        .and_then(|map| label(map.get("field")));
    let reasoning = match (
        object.get("reasoning").and_then(Value::as_bool),
        reasoning_toggle || !reasoning_efforts.is_empty(),
    ) {
        (_, true) => Some(true),
        (declared, false) => declared,
    };
    Some(Model {
        id: id.to_owned(),
        name: label(object.get("name")),
        context,
        max_input: limit_number("input"),
        max_output: limit_number("output"),
        tools: object.get("tool_call").and_then(Value::as_bool),
        reasoning,
        reasoning_efforts,
        reasoning_off_effort,
        reasoning_toggle,
        always_thinking,
        reasoning_field,
        dynamically_loaded_tools: object
            .get("dynamically_loaded_tools")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        image_input: has_input("image"),
        video_input: has_input("video"),
        audio_input: has_input("audio"),
        lifecycle: life,
        wire_override: override_object
            .and_then(|v| v.get("npm").map(|npm| wire_choice(None, Some(npm)))),
        endpoint_override: override_object
            .and_then(|v| v.get("api").map(|api| endpoint(Some(api)))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = "https://models.example/api.json";
    #[test]
    fn explicit_catalog_fetch_is_credential_free_and_honors_egress() {
        use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
        use std::{
            io::{Read, Write},
            net::SocketAddr,
            sync::Arc,
        };
        struct Refuse;
        impl DialGate for Refuse {
            fn permit(
                &self,
                _: DialTarget<'_>,
                _: Option<&ProxyTarget>,
                _: &[SocketAddr],
            ) -> Result<Vec<SocketAddr>, llm_router::ProviderError> {
                Err(llm_router::ProviderError::Connection)
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source = format!("http://{}/api.json", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            fetch(
                &source,
                None,
                Some(Arc::new(Refuse)),
                &llm_router::CancellationToken::new()
            )
            .unwrap_err(),
            CatalogError::Network
        );
        assert!(listener.accept().is_err());
        listener.set_nonblocking(false).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0; 1024];
            while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&chunk[..n]);
            }
            let request = String::from_utf8(raw).unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /api.json "));
            assert!(!request.contains("authorization:"));
            assert!(request.contains("accept: application/json"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        });
        let body = fetch(&source, None, None, &llm_router::CancellationToken::new()).unwrap();
        assert_eq!(body, b"{}");
        assert!(parse(&source, &body, 1).unwrap().providers.is_empty());
        worker.join().unwrap();
    }
    #[test]
    fn catalog_fetch_preserves_the_path_and_never_follows_redirects() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source = format!("http://{}/catalog/", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).unwrap();
            assert!(String::from_utf8_lossy(&bytes[..n]).starts_with("GET /catalog/ "));
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        assert_eq!(
            fetch(&source, None, None, &llm_router::CancellationToken::new()).unwrap_err(),
            CatalogError::HttpStatus(302)
        );
        assert_eq!(parse(&source, b"{}", 1).unwrap().provenance.source, source);
        worker.join().unwrap();
    }
    #[test]
    fn registry_normalizes_capabilities_and_preserves_untrusted_endpoint_decisions() {
        let document = r#"{
          "acme": {"id":"acme","name":"示例", "npm":"@ai-sdk/openai-compatible", "api":"https://api.example/v1", "env":["ACME_KEY","bad name"], "models": {
            "model-one": {"id":"model-one","name":"Model One", "limit":{"context":128000,"input":100000,"output":4000},"tool_call":true,"reasoning_options":[{"type":"effort","values":[null,"low","high"]}],"interleaved":{"field":"reasoning_details"},"modalities":{"input":["text","image"],"output":["text"]}},
            "old": {"id":"old","status":"deprecated","limit":{"context":8000}},
            "override": {"id":"override","limit":{"context":32000},"provider":{"npm":"@ai-sdk/anthropic","api":"https://other.example/v1"}},
            "bad-context": {"id":"bad-context","limit":{"context":0}},
            "audio-only": {"id":"audio-only","limit":{"context":1000},"modalities":{"output":["audio"]}},
            "text-embedding": {"id":"text-embedding","limit":{"context":4096}}
          }},
          "untrusted": {"id":"untrusted", "type":"custom-unknown", "api":"https://${HOST}/v1", "models": {}}
        }"#.as_bytes();
        let registry = parse(SOURCE, document, 123).unwrap();
        assert_eq!(registry.provenance.source, SOURCE);
        assert_eq!(
            registry.provenance.sha256,
            format!("{:x}", Sha256::digest(document))
        );
        assert_eq!(registry.provenance.fetched_at, 123);
        let provider = &registry.providers["acme"];
        assert_eq!(provider.name.as_deref(), Some("示例"));
        assert_eq!(provider.wire, WireChoice::Declared { wire: Wire::OpenAi });
        assert_eq!(
            provider.endpoint,
            Endpoint::Present("https://api.example/v1".into())
        );
        assert_eq!(provider.env_hints, ["ACME_KEY"]);
        assert_eq!(provider.models.len(), 3);
        assert_eq!(provider.models["old"].lifecycle, Lifecycle::Deprecated);
        assert!(provider.models["model-one"].image_input);
        assert_eq!(provider.models["model-one"].max_input, Some(100000));
        assert_eq!(provider.models["model-one"].reasoning, Some(true));
        assert_eq!(
            provider.models["model-one"].reasoning_efforts,
            ["low", "high"]
        );
        assert_eq!(
            provider.models["model-one"].reasoning_off_effort.as_deref(),
            Some("none")
        );
        assert!(!provider.models["model-one"].always_thinking);
        assert_eq!(
            provider.models["model-one"].reasoning_field.as_deref(),
            Some("reasoning_details")
        );
        assert_eq!(
            provider.models["override"].wire_override,
            Some(WireChoice::Declared {
                wire: Wire::Anthropic
            })
        );
        assert_eq!(registry.skipped_models, 3);
        assert_eq!(
            registry.providers["untrusted"].wire,
            WireChoice::Unsupported
        );
        assert_eq!(registry.providers["untrusted"].endpoint, Endpoint::Invalid);
    }
    #[test]
    fn registry_rejects_ambiguous_or_oversized_data_and_invalid_sources() {
        assert_eq!(
            parse("http://remote.example/api", b"{}", 1).unwrap_err(),
            CatalogError::InvalidSource
        );
        assert_eq!(
            parse("https://user:secret@example.com/api", b"{}", 1).unwrap_err(),
            CatalogError::InvalidSource
        );
        assert_eq!(
            parse(SOURCE, &vec![b' '; MAX_CATALOG_BYTES + 1], 1).unwrap_err(),
            CatalogError::TooLarge
        );
        assert_eq!(
            parse(SOURCE, b"[]", 1).unwrap_err(),
            CatalogError::InvalidDocument
        );
        let duplicate = br#"{"p":{"models":{"a":{"id":"same","limit":{"context":10}},"b":{"id":"same","limit":{"context":20}}}}}"#;
        assert_eq!(
            parse(SOURCE, duplicate, 1).unwrap_err(),
            CatalogError::InvalidDocument
        );
        let providers: BTreeMap<String, Value> = (0..=MAX_PROVIDERS)
            .map(|n| (format!("p{n}"), serde_json::json!({"models":{}})))
            .collect();
        let bytes = serde_json::to_vec(&providers).unwrap();
        assert_eq!(
            parse(SOURCE, &bytes, 1).unwrap_err(),
            CatalogError::TooManyProviders
        );
    }
    #[test]
    fn unknown_or_invalid_sdk_metadata_never_becomes_a_silent_native_wire() {
        assert_eq!(
            wire_choice(
                Some(&Value::String("unknown".into())),
                Some(&Value::String("@ai-sdk/openai".into()))
            ),
            WireChoice::Unsupported
        );
        assert_eq!(
            wire_choice(None, Some(&Value::String("@ai-sdk/cohere".into()))),
            WireChoice::Unsupported
        );
        assert_eq!(
            wire_choice(None, Some(&Value::String("custom-sdk".into()))),
            WireChoice::GuessedOpenAi
        );
        assert_eq!(
            wire_choice(None, Some(&Value::Bool(true))),
            WireChoice::Unsupported
        );
        assert_eq!(
            endpoint(Some(&Value::String("https://169.254.169.254/v1".into()))),
            Endpoint::Invalid
        );
    }
}
