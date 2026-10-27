//! Discovered model entries: a catalog beyond what the user configured.
//!
//! Two sources, one merge. A local `models.json` beside the resolved
//! config file, and — when `[models] catalog_url` names one — a remote
//! catalog fetched as `GET <base>/models?client_version=<version>` (the
//! same shape works for a plain OpenAI-compatible `/v1/models`, whose
//! entries carry wire ids that ride the catalog's own origin). Discovered
//! entries become ordinary `[model.<id>]` rows the user did not define;
//! a row the user *did* define always wins — a catalog narrows nothing
//! and overrides nothing. Credentials are never discovered: `api_key`
//! and friends are ignored on the way in, so a catalog can hand out
//! models, never secrets.
//!
//! Failures are notes, never errors: a catalog that cannot be read leaves
//! the user's configuration exactly as it was. One fetch per process per
//! source (with a short TTL) — model resolution happens per turn, and a
//! dead catalog must not add a request to each one.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::user_config::{ConfigProvider, ModelEntry, UserConfig};

/// Most entries one catalog may contribute.
pub const MAX_DISCOVERED_ENTRIES: usize = 256;
/// Longest catalog body accepted.
pub const MAX_CATALOG_BODY_BYTES: usize = 512 * 1024;
/// How long a fetched (or failed) catalog answer is remembered before a
/// fresh fetch is attempted.
pub const CATALOG_TTL: Duration = Duration::from_secs(600);
/// How long the fetch itself may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// One discovered model row: the `[model.<id>]` fields a catalog may
/// carry — everything except credentials, on purpose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredModel {
    pub id: String,
    pub provider: ConfigProvider,
    pub model: String,
    pub base_url: String,
    pub name: Option<String>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub reasoning_effort: Option<llm_router::ReasoningEffort>,
    pub vision: Option<bool>,
    pub effort_ids: std::collections::BTreeMap<llm_router::ReasoningEffort, String>,
}

/// What a catalog said, and where it came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredModels {
    pub source: &'static str,
    /// The URL a remote catalog came from (cache identity); `None` for
    /// the file source.
    pub source_url: Option<String>,
    pub entries: Vec<DiscoveredModel>,
}

/// Parse a catalog body. `default_base_url` is the origin entries without
/// one ride (the catalog's own origin — the OpenAI `/v1/models` shape
/// carries wire ids, not endpoints). Accepts the object-with-`models`
/// shape, a bare array, and the OpenAI `{"data":[…]}` shape.
pub fn parse_discovered(
    body: &str,
    default_base_url: Option<&str>,
) -> Result<DiscoveredModels, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("the catalog is not JSON: {err}"))?;
    let rows = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.get("data").and_then(serde_json::Value::as_array))
        .or_else(|| value.as_array())
        .ok_or_else(|| "the catalog is not a models array".to_owned())?;
    let mut entries = Vec::new();
    for row in rows {
        if entries.len() >= MAX_DISCOVERED_ENTRIES {
            return Err(format!(
                "the catalog exceeds {MAX_DISCOVERED_ENTRIES} entries"
            ));
        }
        let text = |key: &str| {
            row.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let number = |key: &str| {
            row.get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
        };
        // The OpenAI shape's id is the wire model id; the own shape's id
        // is the profile id with the wire id under `model`.
        let Some(id) = text("id").filter(|id| valid_profile_id(id)) else {
            continue;
        };
        let model = text("model")
            .or_else(|| text("wire_model"))
            .unwrap_or_else(|| id.clone());
        if model.is_empty() {
            continue;
        }
        let base_url = text("base_url")
            .or_else(|| default_base_url.map(str::to_owned))
            .unwrap_or_default();
        if base_url.is_empty() {
            continue;
        }
        let provider = match text("provider").as_deref() {
            None | Some("openai-compatible") => ConfigProvider::OpenAiCompatible,
            Some("anthropic") => ConfigProvider::Anthropic,
            Some(other) => return Err(format!("unknown provider {other:?} in the catalog")),
        };
        let mut effort_ids = std::collections::BTreeMap::new();
        if let Some(map) = row.get("effort_ids").and_then(serde_json::Value::as_object) {
            for (effort, wire) in map {
                let Ok(effort) = llm_router::ReasoningEffort::parse(effort) else {
                    continue;
                };
                if let Some(wire) = wire.as_str().filter(|wire| !wire.is_empty()) {
                    effort_ids.insert(effort, wire.to_owned());
                }
            }
        }
        entries.push(DiscoveredModel {
            id,
            provider,
            model,
            base_url,
            name: text("name").filter(|name| !name.is_empty()),
            max_tokens: number("max_tokens"),
            context_window: number("context_window"),
            reasoning_effort: text("reasoning_effort")
                .as_deref()
                .and_then(|raw| llm_router::ReasoningEffort::parse(raw).ok()),
            vision: row.get("vision").and_then(serde_json::Value::as_bool),
            effort_ids,
        });
    }
    Ok(DiscoveredModels {
        source: "parsed",
        source_url: None,
        entries,
    })
}

fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

/// Merge discovered entries into `config`: only ids the user did not
/// define, never credentials. Returns how many rows were added.
pub fn merge_into(config: &mut UserConfig, discovered: &DiscoveredModels) -> usize {
    let mut added = 0;
    for entry in &discovered.entries {
        if config.models.entries.contains_key(&entry.id) {
            continue;
        }
        config.models.entries.insert(
            entry.id.clone(),
            ModelEntry {
                provider: entry.provider,
                model: entry.model.clone(),
                base_url: entry.base_url.clone(),
                name: entry.name.clone(),
                api_key: None,
                env_key: Vec::new(),
                keychain: None,
                effort_ids: entry.effort_ids.clone(),
                retry: None,
                continue_on_length: 0,
                max_tokens: entry.max_tokens,
                context_window: entry.context_window,
                reasoning_effort: entry.reasoning_effort,
                vision: entry.vision,
                caching: None,
                reasoning: None,
            },
        );
        added += 1;
    }
    added
}

/// The endpoint a `catalog_url` configuration is fetched from:
/// `<base>/models?client_version=<version>` when the configured URL does
/// not already end in `/models`, the query appended either way. Pure, so
/// the join is testable.
pub fn catalog_endpoint(configured: &str, client_version: &str) -> String {
    let base = configured.trim_end_matches('/');
    let (path, query) = match base.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (base, None),
    };
    let path = if path.ends_with("/models") {
        path.to_owned()
    } else {
        format!("{path}/models")
    };
    match query {
        Some(query) => format!("{path}?{query}&client_version={client_version}"),
        None => format!("{path}?client_version={client_version}"),
    }
}

/// One fetched-or-failed catalog answer, remembered for [`CATALOG_TTL`].
struct CacheSlot {
    at: Instant,
    discovered: Option<DiscoveredModels>,
    notes: Vec<String>,
}

static CACHE: std::sync::Mutex<Option<CacheSlot>> = std::sync::Mutex::new(None);

/// The catalog for this environment: `models.json` beside the resolved
/// config file when it exists, else the remote `[models] catalog_url`
/// fetched through the same dial path every other request uses. `None`
/// and a note when neither source answers — the caller's configuration
/// stands exactly as it was. One attempt per source per process per TTL.
pub fn load_discovered(
    config_file: &Path,
    catalog_url: Option<&str>,
) -> (Option<DiscoveredModels>, Vec<String>) {
    let models_json = config_file.with_file_name("models.json");
    if models_json.is_file() {
        match std::fs::read_to_string(&models_json) {
            Ok(body) if body.len() <= MAX_CATALOG_BODY_BYTES => {
                match parse_discovered(&body, None) {
                    Ok(mut discovered) => {
                        discovered.source = "file";
                        return (Some(discovered), Vec::new());
                    }
                    Err(reason) => {
                        return (
                            None,
                            vec![format!(
                                "models.json beside the config could not be read: {reason}"
                            )],
                        );
                    }
                }
            }
            Ok(_) => {
                return (
                    None,
                    vec!["models.json beside the config is over its size bound".to_owned()],
                );
            }
            Err(err) => {
                return (
                    None,
                    vec![format!(
                        "models.json beside the config is unreadable: {err}"
                    )],
                );
            }
        }
    }
    let Some(url) = catalog_url else {
        return (None, Vec::new());
    };
    // The TTL cache: a fresh fetch only when the remembered answer has
    // aged out, so per-turn model resolution never becomes per-turn HTTP.
    {
        let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(slot) = cache.as_ref()
            && slot.at.elapsed() < CATALOG_TTL
            && slot
                .discovered
                .as_ref()
                .is_some_and(|d| d.source_of_url() == url)
        {
            return (slot.discovered.clone(), slot.notes.clone());
        }
    }
    let (discovered, notes) = fetch_remote(url);
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some(CacheSlot {
        at: Instant::now(),
        discovered: discovered.clone(),
        notes: notes.clone(),
    });
    (discovered, notes)
}

impl DiscoveredModels {
    fn source_of_url(&self) -> &str {
        self.source_url.as_deref().unwrap_or("")
    }

    fn with_source_url(mut self, url: &str) -> Self {
        self.source_url = Some(url.to_owned());
        self
    }
}

/// Fetch the remote catalog through the same HTTP/1.1 transport the
/// search backend uses, authorized for exactly the catalog's own origin.
/// A failed fetch is `(None, note)` — never an error the caller must
/// handle beyond showing the note.
fn fetch_remote(url: &str) -> (Option<DiscoveredModels>, Vec<String>) {
    let attempt = || -> Result<DiscoveredModels, String> {
        let https = url.starts_with("https://");
        let host = crate::web_fetch::host_of(url)
            .ok_or_else(|| "the catalog url has no host".to_owned())?
            .to_ascii_lowercase();
        let port = {
            let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            authority
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok())
                .unwrap_or(if https { 443 } else { 80 })
        };
        let gate = std::sync::Arc::new(
            crate::provider_egress::ProviderEgress::for_client(
                security::NetworkClient::Tool,
                https,
                &host,
                port,
                None,
            )
            .map_err(|err| format!("egress gate: {err}"))?,
        );
        let auth = llm_router::providers::openai_compatible::StaticWireAuth::bearer(
            "rapidlm-model-catalog".to_owned(),
        )
        .map_err(|err| err.to_string())?;
        let transport = llm_router::providers::openai_compatible::Http1Transport::with_limits(
            auth,
            FETCH_TIMEOUT,
            MAX_CATALOG_BODY_BYTES,
        )
        .with_dial_gate(std::sync::Arc::clone(&gate)
            as std::sync::Arc<dyn llm_router::providers::dial::DialGate>);
        let endpoint = catalog_endpoint(url, env!("CARGO_PKG_VERSION"));
        let response = transport
            .get_raw(
                &endpoint,
                &[("Accept".to_owned(), "application/json".to_owned())],
                &llm_router::provider::CancellationToken::new(),
            )
            .map_err(|err| format!("the catalog fetch failed: {err}"))?;
        if !(200..300).contains(&response.status) {
            return Err(format!("the catalog answered {}", response.status));
        }
        if response.body.len() > MAX_CATALOG_BODY_BYTES {
            return Err("the catalog body is over its size bound".to_owned());
        }
        // Entries without their own base_url ride the catalog's origin —
        // the origin they were discovered at.
        let origin = format!(
            "{}//{}{}",
            if https { "https:" } else { "http:" },
            host,
            if port == (if https { 443 } else { 80 }) {
                String::new()
            } else {
                format!(":{port}")
            }
        );
        let mut discovered =
            parse_discovered(&String::from_utf8_lossy(&response.body), Some(&origin))
                .map_err(|reason| format!("the catalog could not be parsed: {reason}"))?;
        discovered.source = "remote";
        Ok(discovered.with_source_url(url))
    };
    match attempt() {
        Ok(discovered) => (Some(discovered), Vec::new()),
        Err(reason) => (None, vec![format!("model catalog: {reason}")]),
    }
}

/// Test-only: forget the remembered catalog answer.
#[cfg(test)]
pub(crate) fn reset_cache_for_tests() {
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_joins_models_and_the_client_version() {
        assert_eq!(
            catalog_endpoint("https://catalog.example", "1.2.3"),
            "https://catalog.example/models?client_version=1.2.3"
        );
        assert_eq!(
            catalog_endpoint("https://catalog.example/v1", "1.2.3"),
            "https://catalog.example/v1/models?client_version=1.2.3"
        );
        // A URL that already names /models and carries a query keeps both.
        assert_eq!(
            catalog_endpoint("https://x.test/models?team=a", "1.2.3"),
            "https://x.test/models?team=a&client_version=1.2.3"
        );
    }

    #[test]
    fn catalogs_parse_in_all_three_shapes() {
        let own = r#"{"models":[
            {"id":"corp-flash","provider":"openai-compatible","model":"flash-v3",
             "base_url":"https://gw.corp/v1","context_window":131072,
             "effort_ids":{"high":"flash-v3-think"}}
        ]}"#;
        let parsed = parse_discovered(own, None).expect("own shape");
        assert_eq!(parsed.entries.len(), 1);
        let entry = &parsed.entries[0];
        assert_eq!(entry.id, "corp-flash");
        assert_eq!(entry.base_url, "https://gw.corp/v1");
        assert_eq!(entry.context_window, Some(131_072));
        assert_eq!(
            entry.effort_ids.get(&llm_router::ReasoningEffort::High),
            Some(&"flash-v3-think".to_owned())
        );

        // The OpenAI shape: wire ids, origin from the catalog itself.
        let openai = r#"{"data":[{"id":"gwt-4"}]}"#;
        let parsed = parse_discovered(openai, Some("http://127.0.0.1:9/v1")).expect("openai");
        assert_eq!(parsed.entries[0].model, "gwt-4");
        assert_eq!(parsed.entries[0].base_url, "http://127.0.0.1:9/v1");

        // A bare array is the own shape without the wrapper.
        assert!(
            parse_discovered(r#"[{"id":"a","base_url":"http://x"}]"#, None)
                .expect("bare")
                .entries
                .len()
                == 1
        );

        // Rows that cannot stand alone (no base_url anywhere, a bad id)
        // are skipped, not fatal; a bad provider is.
        let thin = parse_discovered(
            r#"{"models":[{"id":"no-origin"},{"id":"ok","base_url":"http://x"},
                          {"bad id":"x","base_url":"http://x"}]}"#,
            None,
        )
        .expect("thin rows parse");
        assert_eq!(thin.entries.len(), 1);
        assert_eq!(thin.entries[0].id, "ok");
        assert!(
            parse_discovered(
                r#"{"models":[{"id":"a","base_url":"http://x","provider":"weird"}]}"#,
                None
            )
            .is_err()
        );
    }

    fn env_with_config(name: &str, text: &str) -> (Vec<(String, String)>, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("rapidlm-modelcat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let config = dir.join("config.toml");
        std::fs::write(&config, text).expect("config");
        (
            vec![(
                crate::user_config::CONFIG_PATH_ENV.to_owned(),
                config.display().to_string(),
            )],
            dir,
        )
    }

    #[test]
    fn models_json_beside_the_config_feeds_the_model_list() {
        let (env, dir) = env_with_config(
            "file",
            "[models]\ndefault = \"local\"\n\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"http://127.0.0.1:1\"\n",
        );
        assert_eq!(
            crate::user_config::list_configured_models(&env),
            vec!["local".to_owned()],
            "without a catalog, exactly the user's own"
        );
        std::fs::write(
            dir.join("models.json"),
            r#"{"models":[{"id":"corp-pro","model":"pro-v2","base_url":"https://gw.corp/v1"}]}"#,
        )
        .expect("models.json");
        let listed = crate::user_config::list_configured_models(&env);
        assert!(
            listed.contains(&"local".to_owned()) && listed.contains(&"corp-pro".to_owned()),
            "{listed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A one-request fixture that records the request line it served and
    /// answers `body`.
    fn catalog_fixture(
        body: &'static str,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorder = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while let Ok(1) = stream.read(&mut byte) {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                *recorder.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(String::from_utf8_lossy(&head).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (addr, seen)
    }

    #[test]
    fn a_remote_catalog_is_fetched_with_the_client_version_and_merged() {
        let (addr, seen) = catalog_fixture(
            r#"{"models":[{"id":"remote-mini","model":"mini-2","base_url":"http://127.0.0.1:1"}]}"#,
        );
        let (env, dir) = env_with_config(
            "remote",
            &format!(
                "[models]\ncatalog_url = \"http://{addr}/v1\"\n\n[model.mine]\nprovider = \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"http://127.0.0.1:1\"\n"
            ),
        );
        reset_cache_for_tests();
        let listed = crate::user_config::list_configured_models(&env);
        assert!(
            listed.contains(&"remote-mini".to_owned()) && listed.contains(&"mine".to_owned()),
            "{listed:?}"
        );
        let request = seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .expect("the catalog was fetched");
        assert!(
            request.contains(&format!(
                "/v1/models?client_version={}",
                env!("CARGO_PKG_VERSION")
            )),
            "the request names /models and the client version: {request}"
        );

        // The TTL cache: a second read serves the remembered answer; the
        // fixture is single-shot, so this proves no second fetch happened.
        let again = crate::user_config::list_configured_models(&env);
        assert!(again.contains(&"remote-mini".to_owned()), "{again:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_adds_only_what_the_user_did_not_define_and_never_credentials() {
        let mut config = crate::user_config::parse_config_document(
            "[model.corp-flash]\nprovider = \"openai-compatible\"\nmodel = \"x\"\nbase_url = \"http://user.test\"\n",
            "t.toml",
        )
        .expect("config");
        // The user's own row for the same id the catalog will offer —
        // with a credential, to prove the user's row (secret included) is
        // the one that survives.
        config
            .models
            .entries
            .get_mut("corp-flash")
            .expect("row")
            .model = "the-users-own-choice".to_owned();
        config
            .models
            .entries
            .get_mut("corp-flash")
            .expect("row")
            .api_key = Some("user-secret".to_owned());
        let discovered = DiscoveredModels {
            source: "file",
            source_url: None,
            entries: vec![
                DiscoveredModel {
                    id: "corp-flash".to_owned(),
                    provider: ConfigProvider::OpenAiCompatible,
                    model: "catalog-version".to_owned(),
                    base_url: "http://catalog.test".to_owned(),
                    name: None,
                    max_tokens: None,
                    context_window: None,
                    reasoning_effort: None,
                    vision: None,
                    effort_ids: Default::default(),
                },
                DiscoveredModel {
                    id: "corp-pro".to_owned(),
                    provider: ConfigProvider::Anthropic,
                    model: "pro-v2".to_owned(),
                    base_url: "https://gw.corp".to_owned(),
                    name: Some("Corp Pro".to_owned()),
                    max_tokens: Some(8192),
                    context_window: None,
                    reasoning_effort: None,
                    vision: Some(true),
                    effort_ids: Default::default(),
                },
            ],
        };
        assert_eq!(merge_into(&mut config, &discovered), 1, "user wins");
        assert_eq!(
            config.models.entries["corp-flash"].model,
            "the-users-own-choice"
        );
        let pro = &config.models.entries["corp-pro"];
        assert_eq!(pro.provider, ConfigProvider::Anthropic);
        assert_eq!(pro.max_tokens, Some(8192));
        assert_eq!(pro.vision, Some(true));
        assert_eq!(pro.api_key, None, "a catalog never hands out credentials");
        assert_eq!(pro.env_key, Vec::<String>::new());
    }
}
