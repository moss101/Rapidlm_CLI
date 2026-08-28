//! Grok Build–style layered model configuration for the `rapid` CLI.
//!
//! Mirrors the Grok Build user-config surface with RapidLM names:
//!
//!   - File: `RAPIDLM_CONFIG` (explicit path, must exist) else the first
//!     existing home candidate `RAPIDLM_HOME/config.toml`,
//!     `$HOME/.rapidlm/config.toml`, `$USERPROFILE/.rapidlm/config.toml`.
//!   - Schema: `[models] default = "<profile-id>"` plus one `[model.<id>]`
//!     table per model (`provider`, `model`, `base_url`, optional `name`,
//!     `api_key`, `env_key`, `max_tokens`, `context_window`,
//!     `reasoning_effort`), and an optional `[phases]` table mapping
//!     request purposes (`chat`, `compact`, …) to `[model.<id>]` ids so
//!     auxiliary phases can ride a cheaper model.
//!   - Precedence: env override `RAPIDLM_MODEL` > `[models].default`.
//!   - Credentials: inline `api_key` wins, else the first set, non-empty
//!     `env_key` entry, else keyless (for local servers without auth).
//!
//! Parsing never reads the process env; callers pass env pairs so the pure
//! core stays testable. Typed errors name keys and never echo credential
//! values. Unknown keys are reported as warnings, matching the kernel
//! loader's document convention.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use llm_router::{parse_purpose_name, purpose_name, PhaseRoute, ReasoningEffort};

/// Env var holding an explicit config file path (Grok: `GROK_CONFIG`).
pub const CONFIG_PATH_ENV: &str = "RAPIDLM_CONFIG";
/// Env var overriding `[models].default` (Grok: model override flags/envs).
pub const DEFAULT_MODEL_ENV: &str = "RAPIDLM_MODEL";
/// Home-root override consumed verbatim (same semantics as `resolve_user_home`).
pub const RAPIDLM_HOME_ENV: &str = "RAPIDLM_HOME";
/// User home root; config lives at `$HOME/.rapidlm/config.toml`.
pub const HOME_ENV: &str = "HOME";
/// Windows user home root; config lives at `$USERPROFILE/.rapidlm/config.toml`.
pub const USERPROFILE_ENV: &str = "USERPROFILE";

/// Maximum accepted config document bytes (mirrors the kernel loader bound).
pub const MAX_USER_CONFIG_BYTES: usize = 256 * 1024;

/// Default provider-side context window when `context_window` is absent.
/// The canonical layer does not enforce it here; it documents the pin.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;
/// Default output cap when `max_tokens` is absent (provider default applies).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// User config document: `[models]` + `[model.<id>]` tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserConfig {
    pub models: ModelsSection,
    /// `[phases]` purpose → model-id overrides for auxiliary model calls.
    pub phases: PhasesSection,
    /// Dotted key paths that were present but not part of the schema.
    pub unknown_keys: Vec<String>,
}

/// `[phases]` section: purpose-name → `[model.<id>]` id.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PhasesSection {
    /// Keyed by canonical llm-router purpose name (`chat`, `compact`, …).
    pub overrides: BTreeMap<String, String>,
}

/// `[models]` section.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelsSection {
    pub default: Option<String>,
    /// Keyed by the `[model.<id>]` table id (the profile id).
    pub entries: BTreeMap<String, ModelEntry>,
}

/// Supported provider adapter kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigProvider {
    OpenAiCompatible,
    Anthropic,
}

impl ConfigProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "openai-compatible" => Some(Self::OpenAiCompatible),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }
}

/// One `[model.<id>]` entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelEntry {
    pub provider: ConfigProvider,
    /// Provider-side model id sent on the wire (e.g. `gpt-4.1`, `llama3.2`).
    pub model: String,
    /// Provider origin (http for local servers, https for TLS-verified
    /// remotes; e.g. `http://127.0.0.1:11434/v1`).
    pub base_url: String,
    /// Optional display name (Grok: `name`).
    pub name: Option<String>,
    /// Inline credential; wins over `env_key` (Grok precedence).
    pub api_key: Option<String>,
    /// Env var names tried in order; first set, non-empty value wins.
    pub env_key: Vec<String>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    /// Reasoning-effort request override; `None` means the provider default.
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// The configured model the exec path should drive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveModel {
    /// `[model.<id>]` table id; must satisfy the llm-router profile alphabet.
    pub profile_id: String,
    pub entry: ModelEntry,
    pub credential: ResolvedCredential,
    /// Purpose → profile routing: overrides from `[phases]`, main = the
    /// selected model (so auxiliary phases fail open to the conversation
    /// model when unconfigured).
    pub phase_route: PhaseRoute,
}

/// Credential resolution outcome for an [`ActiveModel`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCredential {
    /// Plaintext when a credential was configured; `None` means keyless.
    pub plaintext: Option<String>,
    pub source: CredentialSource,
}

/// Where a resolved credential came from (audit/debug only; never the value).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    InlineApiKey,
    EnvVar(String),
    Keyless,
}

/// Result of selection: a configured model, or the typed no-config fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelSelection {
    Configured {
        active: Box<ActiveModel>,
        warnings: Vec<String>,
    },
    Unconfigured { searched: Vec<String> },
}

/// Typed configuration failure. Messages name keys/paths, never secret values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserConfigError {
    /// `RAPIDLM_CONFIG` points at a missing file.
    ExplicitConfigMissing { path: String },
    Unreadable { path: String },
    TooLarge { path: String },
    InvalidUtf8 { path: String },
    Syntax { path: String, message: String },
    TypeMismatch { key: String },
    InvalidValue { key: String, reason: String },
    MissingKey { key: String },
    NoModelsDefined,
    NoDefaultModel { available: Vec<String> },
    UnknownDefaultModel { id: String, available: Vec<String> },
    /// A `[phases]` override names a `[model.<id>]` that does not exist.
    UnknownPhaseModel { key: String, id: String, available: Vec<String> },
}

impl fmt::Display for UserConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitConfigMissing { path } => write!(
                f,
                "RAPIDLM_CONFIG points at a missing config file: {path}"
            ),
            Self::Unreadable { path } => write!(f, "config file is not readable: {path}"),
            Self::TooLarge { path } => {
                write!(f, "config file exceeds {MAX_USER_CONFIG_BYTES} bytes: {path}")
            }
            Self::InvalidUtf8 { path } => write!(f, "config file is not valid UTF-8: {path}"),
            Self::Syntax { path, message } => {
                write!(f, "config file is not valid TOML: {path}: {message}")
            }
            Self::TypeMismatch { key } => write!(f, "config key has the wrong type: {key}"),
            Self::InvalidValue { key, reason } => {
                write!(f, "config key has an invalid value: {key}: {reason}")
            }
            Self::MissingKey { key } => write!(f, "required config key is missing: {key}"),
            Self::NoModelsDefined => write!(
                f,
                "no models defined: add a [model.<id>] table with provider, model, and base_url"
            ),
            Self::NoDefaultModel { available } => write!(
                f,
                "no default model: set [models] default or RAPIDLM_MODEL to one of: {}",
                join_ids(available)
            ),
            Self::UnknownDefaultModel { id, available } => write!(
                f,
                "default model '{id}' has no [model.{id}] table; defined models: {}",
                join_ids(available)
            ),
            Self::UnknownPhaseModel { key, id, available } => write!(
                f,
                "{key} names model '{id}' which has no [model.{id}] table; defined models: {}",
                join_ids(available)
            ),
        }
    }
}

impl std::error::Error for UserConfigError {}

fn join_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        "(none)".to_owned()
    } else {
        ids.join(", ")
    }
}

/// Which config file applies. Explicit env paths must exist; home fallbacks
/// are only used when present, otherwise selection yields [`ModelSelection::
/// Unconfigured`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigSource {
    ExplicitPath(PathBuf),
    HomeFallback(PathBuf),
}

/// Resolve the applicable config path from env pairs (pure; no process env).
pub fn resolve_config_source(env: &[(String, String)]) -> ConfigSource {
    if let Some(path) = env_value(env, CONFIG_PATH_ENV)
        && !path.trim().is_empty()
    {
        return ConfigSource::ExplicitPath(PathBuf::from(path));
    }
    let home = |name: &str, join: fn(&Path) -> PathBuf| -> Option<PathBuf> {
        let value = env_value(env, name)?;
        if value.trim().is_empty() {
            return None;
        }
        let candidate = join(Path::new(value));
        candidate.is_file().then_some(candidate)
    };
    let rapidlm_home = |root: &Path| root.join("config.toml");
    let dot_rapidlm = |root: &Path| root.join(".rapidlm").join("config.toml");
    for (name, join) in [
        (RAPIDLM_HOME_ENV, rapidlm_home as fn(&Path) -> PathBuf),
        (HOME_ENV, dot_rapidlm),
        (USERPROFILE_ENV, dot_rapidlm),
    ] {
        if let Some(path) = home(name, join) {
            return ConfigSource::HomeFallback(path);
        }
    }
    // No existing candidate: report the HOME convention in the typed fallback.
    ConfigSource::HomeFallback(
        env_value(env, HOME_ENV)
            .map(|home| Path::new(home).join(".rapidlm").join("config.toml"))
            .unwrap_or_else(|| PathBuf::from(".rapidlm/config.toml")),
    )
}

pub(crate) fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Read and parse the config file for a source. Missing explicit paths are
/// typed failures; a missing home fallback returns `Ok(None)` so callers can
/// take the typed unconfigured path.
pub fn load_config(source: &ConfigSource) -> Result<Option<UserConfig>, UserConfigError> {
    let path = match source {
        ConfigSource::ExplicitPath(path) => {
            if !path.is_file() {
                return Err(UserConfigError::ExplicitConfigMissing {
                    path: path.display().to_string(),
                });
            }
            path
        }
        ConfigSource::HomeFallback(path) => {
            if !path.is_file() {
                return Ok(None);
            }
            path
        }
    };
    read_config_file(path).map(Some)
}

/// Read and parse one config document.
pub fn read_config_file(path: &Path) -> Result<UserConfig, UserConfigError> {
    let bytes = fs::read(path).map_err(|_| UserConfigError::Unreadable {
        path: path.display().to_string(),
    })?;
    let shown = path.display().to_string();
    if bytes.len() > MAX_USER_CONFIG_BYTES {
        return Err(UserConfigError::TooLarge { path: shown });
    }
    let body = String::from_utf8(bytes).map_err(|_| UserConfigError::InvalidUtf8 {
        path: shown.clone(),
    })?;
    parse_config_document(&body, &shown)
}

/// Parse a config document body (pure).
pub fn parse_config_document(body: &str, path: &str) -> Result<UserConfig, UserConfigError> {
    let parsed: toml::Value = toml::from_str(body)
        .map_err(|err| UserConfigError::Syntax {
            path: path.to_owned(),
            message: err.message().to_owned(),
        })?;
    let root = parsed.as_table().ok_or(UserConfigError::TypeMismatch {
        key: "(document root)".to_owned(),
    })?;

    let mut unknown_keys = Vec::new();
    for key in root.keys() {
        if key != "models" && key != "model" && key != "phases" {
            unknown_keys.push(key.clone());
        }
    }

    let mut models = ModelsSection::default();
    if let Some(section) = root.get("models") {
        let table = expect_table(section, "models")?;
        for key in table.keys() {
            if key != "default" {
                unknown_keys.push(format!("models.{key}"));
            }
        }
        if let Some(value) = table.get("default") {
            models.default = Some(expect_non_empty_str(value, "models.default")?.to_owned());
        }
    }

    if let Some(section) = root.get("model") {
        let table = expect_table(section, "model")?;
        for (id, value) in table {
            if id.is_empty() {
                return Err(UserConfigError::InvalidValue {
                    key: "model.<id>".to_owned(),
                    reason: "table id must not be empty".to_owned(),
                });
            }
            let entry_table = expect_table(value, &format!("model.{id}"))?;
            models
                .entries
                .insert(id.clone(), parse_model_entry(id, entry_table, &mut unknown_keys)?);
        }
    }

    let mut phases = PhasesSection::default();
    if let Some(section) = root.get("phases") {
        let table = expect_table(section, "phases")?;
        for (key, value) in table {
            let Some(purpose) = parse_purpose_name(key) else {
                unknown_keys.push(format!("phases.{key}"));
                continue;
            };
            let id = expect_non_empty_str(value, &format!("phases.{key}"))?.to_owned();
            phases.overrides.insert(purpose_name(purpose).to_owned(), id);
        }
    }

    Ok(UserConfig {
        models,
        phases,
        unknown_keys,
    })
}

fn parse_model_entry(
    id: &str,
    table: &toml::Table,
    unknown_keys: &mut Vec<String>,
) -> Result<ModelEntry, UserConfigError> {
    let prefix = format!("model.{id}");
    let known = [
        "provider",
        "model",
        "base_url",
        "name",
        "api_key",
        "env_key",
        "max_tokens",
        "context_window",
        "reasoning_effort",
    ];
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            unknown_keys.push(format!("{prefix}.{key}"));
        }
    }

    let missing = |key: &str| UserConfigError::MissingKey {
        key: format!("{prefix}.{key}"),
    };
    let provider_raw = table
        .get("provider")
        .ok_or_else(|| missing("provider"))?
        .as_str()
        .ok_or(UserConfigError::TypeMismatch {
            key: format!("{prefix}.provider"),
        })?;
    let provider = ConfigProvider::parse(provider_raw).ok_or(UserConfigError::InvalidValue {
        key: format!("{prefix}.provider"),
        reason: "expected \"openai-compatible\" or \"anthropic\"".to_owned(),
    })?;

    let model = expect_non_empty_str(
        table.get("model").ok_or_else(|| missing("model"))?,
        &format!("{prefix}.model"),
    )?
    .to_owned();

    let base_url = expect_non_empty_str(
        table.get("base_url").ok_or_else(|| missing("base_url"))?,
        &format!("{prefix}.base_url"),
    )?
    .to_owned();

    let name = match table.get("name") {
        Some(value) => Some(expect_non_empty_str(value, &format!("{prefix}.name"))?.to_owned()),
        None => None,
    };

    let api_key = match table.get("api_key") {
        Some(value) => Some(expect_non_empty_str(value, &format!("{prefix}.api_key"))?.to_owned()),
        None => None,
    };

    let env_key = match table.get("env_key") {
        None => Vec::new(),
        Some(toml::Value::String(raw)) => vec![validate_env_name(raw, &format!("{prefix}.env_key"))?],
        Some(toml::Value::Array(items)) => {
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                let raw = item.as_str().ok_or(UserConfigError::TypeMismatch {
                    key: format!("{prefix}.env_key"),
                })?;
                names.push(validate_env_name(raw, &format!("{prefix}.env_key"))?);
            }
            names
        }
        Some(_) => {
            return Err(UserConfigError::TypeMismatch {
                key: format!("{prefix}.env_key"),
            });
        }
    };

    let max_tokens = match table.get("max_tokens") {
        None => None,
        Some(value) => Some(positive_u32(value, &format!("{prefix}.max_tokens"))?),
    };
    let context_window = match table.get("context_window") {
        None => None,
        Some(value) => Some(positive_u32(value, &format!("{prefix}.context_window"))?),
    };
    let reasoning_effort = match table.get("reasoning_effort") {
        None => None,
        Some(value) => {
            let raw = value.as_str().ok_or(UserConfigError::TypeMismatch {
                key: format!("{prefix}.reasoning_effort"),
            })?;
            Some(ReasoningEffort::parse(raw).map_err(|_| UserConfigError::InvalidValue {
                key: format!("{prefix}.reasoning_effort"),
                reason: "expected none|minimal|low|medium|high|xhigh|ultra".to_owned(),
            })?)
        }
    };

    Ok(ModelEntry {
        provider,
        model,
        base_url,
        name,
        api_key,
        env_key,
        max_tokens,
        context_window,
        reasoning_effort,
    })
}

fn validate_env_name(raw: &str, key: &str) -> Result<String, UserConfigError> {
    let valid = !raw.is_empty()
        && raw.len() <= 256
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !valid {
        return Err(UserConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "expected an env var name of letters, digits, and underscores".to_owned(),
        });
    }
    Ok(raw.to_owned())
}

fn positive_u32(value: &toml::Value, key: &str) -> Result<u32, UserConfigError> {
    let raw = value.as_integer().ok_or(UserConfigError::TypeMismatch {
        key: key.to_owned(),
    })?;
    if raw <= 0 || raw > u32::MAX as i64 {
        return Err(UserConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "expected a positive integer".to_owned(),
        });
    }
    Ok(raw as u32)
}

fn expect_table<'a>(
    value: &'a toml::Value,
    key: &str,
) -> Result<&'a toml::Table, UserConfigError> {
    value.as_table().ok_or(UserConfigError::TypeMismatch {
        key: key.to_owned(),
    })
}

fn expect_non_empty_str<'a>(
    value: &'a toml::Value,
    key: &str,
) -> Result<&'a str, UserConfigError> {
    let raw = value.as_str().ok_or(UserConfigError::TypeMismatch {
        key: key.to_owned(),
    })?;
    if raw.is_empty() {
        return Err(UserConfigError::InvalidValue {
            key: key.to_owned(),
            reason: "must not be empty".to_owned(),
        });
    }
    Ok(raw)
}

/// Apply the Grok precedence: `RAPIDLM_MODEL` > `[models].default`, then
/// resolve the entry and its credential (`api_key` > first non-empty
/// `env_key` > keyless).
pub fn resolve_active(env: &[(String, String)], config: &UserConfig) -> Result<ActiveModel, UserConfigError> {
    if config.models.entries.is_empty() {
        return Err(UserConfigError::NoModelsDefined);
    }
    let available: Vec<String> = config.models.entries.keys().cloned().collect();
    let override_id = env_value(env, DEFAULT_MODEL_ENV);
    if let Some(raw) = override_id
        && raw.trim().is_empty()
    {
        return Err(UserConfigError::InvalidValue {
            key: DEFAULT_MODEL_ENV.to_owned(),
            reason: "must not be empty".to_owned(),
        });
    }
    let default_id = match override_id {
        Some(id) => id.to_owned(),
        None => match config.models.default.clone() {
            Some(id) => id,
            None => {
                return Err(UserConfigError::NoDefaultModel { available });
            }
        },
    };
    let entry = config
        .models
        .entries
        .get(&default_id)
        .ok_or_else(|| UserConfigError::UnknownDefaultModel {
            id: default_id.clone(),
            available: available.clone(),
        })?;
    // `[phases]` overrides must name defined models; an unknown purpose name
    // was already demoted to a warning at parse time (typo tolerance), but a
    // missing target model is a hard error, like the default model.
    let mut route = llm_router::ProfileId::parse(&default_id)
        .map(PhaseRoute::new)
        .map_err(|_| UserConfigError::InvalidValue {
            key: format!("[models] default '{default_id}'"),
            reason: "must satisfy the llm-router profile alphabet".to_owned(),
        })?;
    for (purpose_name, id) in &config.phases.overrides {
        if !config.models.entries.contains_key(id) {
            return Err(UserConfigError::UnknownPhaseModel {
                key: format!("phases.{purpose_name}"),
                id: id.clone(),
                available: available.clone(),
            });
        }
        if let Some(purpose) = parse_purpose_name(purpose_name) {
            let profile = llm_router::ProfileId::parse(id).map_err(|_| {
                UserConfigError::InvalidValue {
                    key: format!("phases.{purpose_name}"),
                    reason: "must satisfy the llm-router profile alphabet".to_owned(),
                }
            })?;
            route = route.with_override(purpose, profile);
        }
    }
    Ok(ActiveModel {
        profile_id: default_id,
        entry: entry.clone(),
        credential: resolve_credential(entry, env),
        phase_route: route,
    })
}

/// Resolve the model driving one request purpose: the `[phases]` override
/// entry when configured, else the default model. Auxiliary phases fail open
/// to the conversation model by construction of [`ActiveModel::phase_route`].
pub fn resolve_purpose_model(
    env: &[(String, String)],
    config: &UserConfig,
    purpose: llm_router::provider::ModelPurpose,
) -> Result<ActiveModel, UserConfigError> {
    let active = resolve_active(env, config)?;
    let routed = active.phase_route.route(purpose).as_str();
    if routed == active.profile_id {
        return Ok(active);
    }
    let entry = config.models.entries.get(routed).ok_or_else(|| {
        UserConfigError::UnknownPhaseModel {
            key: format!("phases.{}", llm_router::purpose_name(purpose)),
            id: routed.to_owned(),
            available: config.models.entries.keys().cloned().collect(),
        }
    })?;
    Ok(ActiveModel {
        profile_id: routed.to_owned(),
        entry: entry.clone(),
        credential: resolve_credential(entry, env),
        phase_route: active.phase_route,
    })
}

/// Grok credential precedence: inline `api_key`, then the first set,
/// non-empty `env_key` entry, then keyless.
pub fn resolve_credential(entry: &ModelEntry, env: &[(String, String)]) -> ResolvedCredential {
    if let Some(api_key) = &entry.api_key {
        return ResolvedCredential {
            plaintext: Some(api_key.clone()),
            source: CredentialSource::InlineApiKey,
        };
    }
    for name in &entry.env_key {
        if let Some(value) = env_value(env, name)
            && !value.is_empty()
        {
            return ResolvedCredential {
                plaintext: Some(value.to_owned()),
                source: CredentialSource::EnvVar(name.clone()),
            };
        }
    }
    ResolvedCredential {
        plaintext: None,
        source: CredentialSource::Keyless,
    }
}

/// Selection for the exec path: load the applicable document, resolve the
/// active model, or report the typed no-config fallback (never a guess).
pub fn select_active_model(env: &[(String, String)]) -> Result<ModelSelection, UserConfigError> {
    let source = resolve_config_source(env);
    let searched = match &source {
        ConfigSource::ExplicitPath(path) | ConfigSource::HomeFallback(path) => {
            vec![path.display().to_string()]
        }
    };
    let Some(config) = load_config(&source)? else {
        return Ok(ModelSelection::Unconfigured { searched });
    };
    let warnings = config
        .unknown_keys
        .iter()
        .map(|key| format!("unknown config key '{key}'"))
        .collect();
    let active = Box::new(resolve_active(env, &config)?);
    Ok(ModelSelection::Configured { active, warnings })
}

/// Process-env entry point used by the CLI exec command.
pub fn select_from_process_env() -> Result<ModelSelection, UserConfigError> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    select_active_model(&env)
}

/// Managed-policy-aware selection: the enterprise layer (`RAPIDLM_MANAGED_CONFIG`)
/// gates the resolution after the normal user resolution. Gate enforcement is
/// reported as warnings; allowlist violations are typed field errors.
pub fn select_active_model_gated(
    env: &[(String, String)],
) -> Result<ModelSelection, crate::managed_config::GatedConfigError> {
    let source = resolve_config_source(env);
    let searched = match &source {
        ConfigSource::ExplicitPath(path) | ConfigSource::HomeFallback(path) => {
            vec![path.display().to_string()]
        }
    };
    let Some(config) = load_config(&source)? else {
        return Ok(ModelSelection::Unconfigured { searched });
    };
    let mut warnings = config
        .unknown_keys
        .iter()
        .map(|key| format!("unknown config key '{key}'"))
        .collect::<Vec<_>>();
    let policy = crate::managed_config::load_policy(env)?;
    let gated = crate::managed_config::resolve_gated(env, &config, policy.as_ref())?;
    for report in &gated.reports {
        warnings.push(format!("managed gate: {report}"));
    }
    Ok(ModelSelection::Configured {
        active: Box::new(gated.active),
        warnings,
    })
}

/// Process-env entry point for the managed-policy-aware selection.
pub fn select_from_process_env_gated(
) -> Result<ModelSelection, crate::managed_config::GatedConfigError> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    select_active_model_gated(&env)
}

#[allow(dead_code)]
fn _assert_error_traits() {
    fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}
    assert_error::<UserConfigError>();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const VALID_DOC: &str = r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"
name = "Ollama local"
env_key = "OLLAMA_API_KEY"
max_tokens = 2048
context_window = 65536

[model.cloud]
provider = "anthropic"
model = "claude-3-5-sonnet"
base_url = "http://gateway.internal:8080"
api_key = "inline-secret"
"#;

    #[test]
    fn parse_valid_document_with_multiple_models() {
        let config = parse_config_document(VALID_DOC, "test.toml").expect("parse");
        assert_eq!(config.models.default.as_deref(), Some("local"));
        assert_eq!(config.models.entries.len(), 2);
        let local = &config.models.entries["local"];
        assert_eq!(local.provider, ConfigProvider::OpenAiCompatible);
        assert_eq!(local.model, "llama3.2");
        assert_eq!(local.env_key, vec!["OLLAMA_API_KEY".to_owned()]);
        assert_eq!(local.max_tokens, Some(2048));
        assert_eq!(local.context_window, Some(65536));
        assert!(config.unknown_keys.is_empty());
    }

    #[test]
    fn parse_reports_unknown_keys_as_warnings() {
        let doc = r#"
[models]
default = "a"
typo_key = true

[model.a]
provider = "openai-compatible"
model = "m"
base_url = "http://127.0.0.1:1"
env_ky = "X"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        assert_eq!(config.unknown_keys, vec!["models.typo_key", "model.a.env_ky"]);
    }

    #[test]
    fn parse_reasoning_effort_and_phase_overrides() {
        let doc = r#"
[models]
default = "local"

[phases]
compact = "cloud"
review = "local"

[model.local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"
reasoning_effort = "high"

[model.cloud]
provider = "anthropic"
model = "claude-3-5-sonnet"
base_url = "http://gateway.internal:8080"
api_key = "inline-secret"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        assert_eq!(
            config.models.entries["local"].reasoning_effort,
            Some(ReasoningEffort::High)
        );
        assert_eq!(config.phases.overrides.len(), 2);
        assert_eq!(config.phases.overrides["compact"], "cloud");
        assert!(config.unknown_keys.is_empty());
    }

    #[test]
    fn phases_unknown_purpose_name_is_a_warning_not_an_error() {
        let doc = r#"
[models]
default = "a"

[phases]
sumarize = "a"

[model.a]
provider = "openai-compatible"
model = "m"
base_url = "http://127.0.0.1:1"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        assert_eq!(config.unknown_keys, vec!["phases.sumarize"]);
        assert!(config.phases.overrides.is_empty());
    }

    #[test]
    fn reasoning_effort_invalid_value_is_typed() {
        let doc = r#"
[model.a]
provider = "openai-compatible"
model = "m"
base_url = "http://127.0.0.1:1"
reasoning_effort = "maximum"
"#;
        let err = parse_config_document(doc, "test.toml").expect_err("invalid effort");
        assert!(err.to_string().contains("model.a.reasoning_effort"));
    }

    #[test]
    fn resolve_active_builds_phase_route_and_validates_targets() {
        let config =
            parse_config_document(PHASES_DOC, "test.toml").expect("parse");
        let active = resolve_active(&[], &config).expect("resolve");
        assert_eq!(active.profile_id, "local");
        assert_eq!(
            active.phase_route.route(llm_router::provider::ModelPurpose::Compact)
                .as_str(),
            "cloud"
        );
        assert_eq!(
            active.phase_route.route(llm_router::provider::ModelPurpose::Chat).as_str(),
            "local"
        );
        // The default-model env override also re-roots the phase route.
        let env = env(&[("RAPIDLM_MODEL", "cloud")]);
        let active = resolve_active(&env, &config).expect("resolve override");
        assert_eq!(
            active.phase_route.route(llm_router::provider::ModelPurpose::Chat).as_str(),
            "cloud"
        );
    }

    #[test]
    fn phase_override_to_undefined_model_is_a_hard_error() {
        let doc = r#"
[models]
default = "local"

[phases]
compact = "missing"

[model.local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        let err = resolve_active(&[], &config).expect_err("unknown phase target");
        assert!(err
            .to_string()
            .contains("phases.compact names model 'missing'"));
    }

    /// Two-model document with a compact-phase override to the cloud entry.
    const PHASES_DOC: &str = r#"
[models]
default = "local"

[phases]
compact = "cloud"

[model.local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"

[model.cloud]
provider = "anthropic"
model = "claude-3-5-sonnet"
base_url = "http://gateway.internal:8080"
api_key = "inline-secret"
"#;

    #[test]
    fn parse_rejects_unknown_provider_and_bad_scalars() {
        let doc = r#"
[model.a]
provider = "vllm-ish"
model = "m"
base_url = "http://127.0.0.1:1"
"#;
        let err = parse_config_document(doc, "test.toml").expect_err("provider");
        assert_eq!(
            err,
            UserConfigError::InvalidValue {
                key: "model.a.provider".to_owned(),
                reason: "expected \"openai-compatible\" or \"anthropic\"".to_owned(),
            }
        );

        let doc = r#"
[model.a]
provider = "anthropic"
model = "m"
base_url = "http://127.0.0.1:1"
max_tokens = 0
"#;
        let err = parse_config_document(doc, "test.toml").expect_err("max_tokens");
        assert!(matches!(err, UserConfigError::InvalidValue { .. }));

        let doc = "[models]\ndefault = 3\n";
        let err = parse_config_document(doc, "test.toml").expect_err("type");
        assert_eq!(
            err,
            UserConfigError::TypeMismatch {
                key: "models.default".to_owned()
            }
        );
    }

    #[test]
    fn parse_rejects_empty_and_mistyped_required_strings() {
        let doc = r#"
[model.a]
provider = "anthropic"
model = ""
base_url = "http://127.0.0.1:1"
"#;
        let err = parse_config_document(doc, "test.toml").expect_err("empty model");
        assert!(matches!(err, UserConfigError::InvalidValue { .. }));

        let doc = r#"
[model.a]
provider = "anthropic"
base_url = "http://127.0.0.1:1"
"#;
        let err = parse_config_document(doc, "test.toml").expect_err("missing model");
        assert_eq!(
            err,
            UserConfigError::MissingKey {
                key: "model.a.model".to_owned()
            }
        );
    }

    #[test]
    fn env_key_accepts_string_or_array() {
        let doc = r#"
[model.a]
provider = "openai-compatible"
model = "m"
base_url = "http://127.0.0.1:1"
env_key = ["A_KEY", "B_KEY"]
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        assert_eq!(
            config.models.entries["a"].env_key,
            vec!["A_KEY".to_owned(), "B_KEY".to_owned()]
        );

        let doc = r#"
[model.a]
provider = "openai-compatible"
model = "m"
base_url = "http://127.0.0.1:1"
env_key = "not a name!"
"#;
        let err = parse_config_document(doc, "test.toml").expect_err("bad env name");
        assert!(matches!(err, UserConfigError::InvalidValue { .. }));
    }

    #[test]
    fn resolve_prefers_env_override_then_config_default() {
        let config = parse_config_document(VALID_DOC, "test.toml").expect("parse");

        let active = resolve_active(&env(&[]), &config).expect("resolve");
        assert_eq!(active.profile_id, "local");

        let active =
            resolve_active(&env(&[(DEFAULT_MODEL_ENV, "cloud")]), &config).expect("resolve");
        assert_eq!(active.profile_id, "cloud");
        assert_eq!(active.entry.provider, ConfigProvider::Anthropic);
        assert_eq!(active.credential.source, CredentialSource::InlineApiKey);
    }

    #[test]
    fn resolve_credential_precedence_api_key_then_env_then_keyless() {
        let entry = &parse_config_document(VALID_DOC, "test.toml")
            .expect("parse")
            .models
            .entries["local"];
        // api_key absent; first set, non-empty env entry wins.
        let resolved = resolve_credential(entry, &env(&[("B_KEY", "b"), ("OLLAMA_API_KEY", "k")]));
        assert_eq!(
            resolved.source,
            CredentialSource::EnvVar("OLLAMA_API_KEY".to_owned())
        );
        // Empty values are skipped, not selected.
        let resolved = resolve_credential(entry, &env(&[("OLLAMA_API_KEY", "")]));
        assert_eq!(resolved.source, CredentialSource::Keyless);
        assert!(resolved.plaintext.is_none());

        let doc = r#"
[model.a]
provider = "openai-compatible"
model = "m"
base_url = "http://127.0.0.1:1"
env_key = ["MISSING_A", "PRESENT_B"]
"#;
        let entry = &parse_config_document(doc, "test.toml")
            .expect("parse")
            .models
            .entries["a"];
        let resolved = resolve_credential(entry, &env(&[("PRESENT_B", "second-wins")]));
        assert_eq!(
            resolved.source,
            CredentialSource::EnvVar("PRESENT_B".to_owned())
        );
    }

    #[test]
    fn resolve_typed_failures_for_default_selection() {
        let config = parse_config_document(VALID_DOC, "test.toml").expect("parse");
        let err = resolve_active(&env(&[(DEFAULT_MODEL_ENV, "missing")]), &config)
            .expect_err("unknown default");
        assert_eq!(
            err,
            UserConfigError::UnknownDefaultModel {
                id: "missing".to_owned(),
                available: vec!["cloud".to_owned(), "local".to_owned()],
            }
        );

        let doc = "[model.a]\nprovider = \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"http://127.0.0.1:1\"\n";
        let config = parse_config_document(doc, "test.toml").expect("parse");
        let err = resolve_active(&env(&[]), &config).expect_err("no default");
        assert!(matches!(err, UserConfigError::NoDefaultModel { .. }));

        let err = resolve_active(
            &env(&[(DEFAULT_MODEL_ENV, "   ")]),
            &parse_config_document(VALID_DOC, "t").expect("parse"),
        )
        .expect_err("empty override");
        assert!(matches!(err, UserConfigError::InvalidValue { key, .. } if key == DEFAULT_MODEL_ENV));
    }

    #[test]
    fn config_path_env_wins_and_must_exist_for_selection() {
        // Explicit path that exists is used verbatim.
        let dir = std::env::temp_dir().join(format!("rapidlm-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("explicit.toml");
        fs::write(&file, VALID_DOC).expect("write");
        let source = resolve_config_source(&env(&[(CONFIG_PATH_ENV, file.to_str().unwrap())]));
        assert_eq!(source, ConfigSource::ExplicitPath(file.clone()));
        let loaded = load_config(&source).expect("load").expect("some");
        assert_eq!(loaded.models.entries.len(), 2);

        // Explicit path missing is a typed failure, not a silent fallback.
        let missing = dir.join("absent.toml");
        let selection = select_active_model(&env(&[(CONFIG_PATH_ENV, missing.to_str().unwrap())]));
        assert!(matches!(
            selection,
            Err(UserConfigError::ExplicitConfigMissing { .. })
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn home_fallback_missing_yields_unconfigured() {
        let dir = std::env::temp_dir().join(format!("rapidlm-empty-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        let selection = select_active_model(&env(&[(HOME_ENV, dir.to_str().unwrap())]))
            .expect("selection");
        match selection {
            ModelSelection::Unconfigured { searched } => {
                assert_eq!(searched.len(), 1);
                assert!(searched[0].ends_with(".rapidlm/config.toml"));
            }
            other => panic!("expected unconfigured, got {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn home_fallback_present_selects_configured_model() {
        let dir = std::env::temp_dir().join(format!("rapidlm-home-{}", std::process::id()));
        let config_dir = dir.join(".rapidlm");
        fs::create_dir_all(&config_dir).expect("mkdir");
        fs::write(config_dir.join("config.toml"), VALID_DOC).expect("write");
        let selection =
            select_active_model(&env(&[(HOME_ENV, dir.to_str().unwrap())])).expect("selection");
        match selection {
            ModelSelection::Configured { active, warnings } => {
                assert_eq!(active.profile_id, "local");
                assert!(warnings.is_empty());
            }
            other => panic!("expected configured, got {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rapidl_home_root_is_used_verbatim() {
        let dir = std::env::temp_dir().join(format!("rapidlm-root-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("config.toml"), VALID_DOC).expect("write");
        let source = resolve_config_source(&env(&[(RAPIDLM_HOME_ENV, dir.to_str().unwrap())]));
        assert_eq!(source, ConfigSource::HomeFallback(dir.join("config.toml")));
        let selection = select_active_model(&env(&[(RAPIDLM_HOME_ENV, dir.to_str().unwrap())]))
            .expect("selection");
        assert!(matches!(selection, ModelSelection::Configured { .. }));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn error_display_never_contains_secret_values() {
        let doc = r#"
[model.a]
provider = "openai-compatible"
model = "m"
base_url = "http://127.0.0.1:1"
api_key = "super-secret-value"
env_key = 42
"#;
        let err = parse_config_document(doc, "test.toml").expect_err("type");
        let rendered = format!("{err}");
        assert!(!rendered.contains("super-secret-value"));
        assert!(rendered.contains("model.a.env_key"));
    }
}
