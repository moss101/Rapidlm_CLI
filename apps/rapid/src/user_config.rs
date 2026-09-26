//! Layered model configuration for the `rapid` CLI:
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
//!   - Proxy: `[network] proxy = "environment"` (or `RAPIDLM_PROXY`, which
//!     wins) routes model connections through the proxy the standard proxy
//!     variables name; the default, `"none"`, dials directly and reads none
//!     of them (S11: an exported `HTTPS_PROXY` changes nothing unasked).
//!
//! Parsing never reads the process env; callers pass env pairs so the pure
//! core stays testable. Typed errors name keys and never echo credential
//! values. Unknown keys are reported as warnings, matching the kernel
//! loader's document convention.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use llm_router::{PhaseRoute, ReasoningEffort, parse_purpose_name, purpose_name};

/// Env var holding an explicit config file path.
pub const CONFIG_PATH_ENV: &str = "RAPIDLM_CONFIG";
/// Env var overriding `[models].default`.
pub const DEFAULT_MODEL_ENV: &str = "RAPIDLM_MODEL";
/// Home-root override consumed verbatim (same semantics as `resolve_user_home`).
pub const RAPIDLM_HOME_ENV: &str = "RAPIDLM_HOME";
/// User home root; config lives at `$HOME/.rapidlm/config.toml`.
pub const HOME_ENV: &str = "HOME";
/// Windows user home root; config lives at `$USERPROFILE/.rapidlm/config.toml`.
pub const USERPROFILE_ENV: &str = "USERPROFILE";
/// Env var overriding `[network] proxy` (`environment` | `none`).
pub const PROXY_MODE_ENV: &str = "RAPIDLM_PROXY";

/// Maximum accepted config document bytes (mirrors the kernel loader bound).
pub const MAX_USER_CONFIG_BYTES: usize = 256 * 1024;

/// Default provider-side context window when `context_window` is absent.
/// The canonical layer does not enforce it here; it documents the pin.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;
/// Default output cap when `max_tokens` is absent (provider default applies).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
/// Maximum `[models] fallback` entries accepted (matches
/// `llm_router::fallback::MAX_FALLBACK_MODELS` minus the primary).
pub const MAX_FALLBACK_MODELS: usize = 7;

/// User config document: `[models]` + `[model.<id>]` tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserConfig {
    pub models: ModelsSection,
    /// `[phases]` purpose → model-id overrides for auxiliary model calls.
    pub phases: PhasesSection,
    /// `[network]` section: how model connections leave the machine.
    pub network: NetworkSection,
    /// Dotted key paths that were present but not part of the schema.
    pub unknown_keys: Vec<String>,
}

/// `[network]` section.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetworkSection {
    /// `proxy`: absent reads as [`ProxyMode::None`].
    pub proxy: Option<ProxyMode>,
}

/// Whether model connections go through the proxy the environment names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyMode {
    /// Dial directly; the proxy variables are not read (the default).
    None,
    /// Read `https_proxy` / `HTTPS_PROXY`, `http_proxy`, `no_proxy` /
    /// `NO_PROXY` and dial through the proxy they name.
    Environment,
}

impl ProxyMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "none" => Some(Self::None),
            "environment" => Some(Self::Environment),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Environment => "environment",
        }
    }
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
    /// Ordered `[model.<id>]` table ids to fall back onto, in order, when the
    /// default model's provider fails. Explicit and user-owned: never
    /// inferred from the other configured entries. Empty means no fallback
    /// (today's behavior — a provider failure surfaces to the user as it
    /// always has).
    pub fallback: Vec<String>,
}

/// Most `continue_on_length` continuations accepted.
pub const MAX_CONTINUATIONS: u32 = 8;
/// Longest `retry.max_attempts` accepted (the first attempt included).
pub const MAX_RETRY_ATTEMPTS: u32 = 11;
/// Longest `retry.base_ms` / `retry.max_ms` accepted.
pub const MAX_RETRY_WAIT_MS: u64 = 600_000;

/// A class of step failure a retry policy may retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryClass {
    /// The provider said to slow down (HTTP 429).
    RateLimit,
    /// The provider failed on its side (5xx, a dropped stream).
    Server,
    /// The endpoint could not be reached, or the connection broke.
    Network,
    /// The provider rejected an already-shaped request (other 4xx).
    Rejected,
}

impl RetryClass {
    pub const ALL: [Self; 4] = [Self::RateLimit, Self::Server, Self::Network, Self::Rejected];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Server => "server",
            Self::Network => "network",
            Self::Rejected => "rejected",
        }
    }
}

/// The classes a policy retries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryClasses {
    pub rate_limit: bool,
    pub server: bool,
    pub network: bool,
    pub rejected: bool,
}

impl RetryClasses {
    pub const ALL: Self = Self {
        rate_limit: true,
        server: true,
        network: true,
        rejected: true,
    };

    pub const fn allows(self, class: RetryClass) -> bool {
        match class {
            RetryClass::RateLimit => self.rate_limit,
            RetryClass::Server => self.server,
            RetryClass::Network => self.network,
            RetryClass::Rejected => self.rejected,
        }
    }
}

/// `[model.<id>] retry`: how the step layer retries this model's failures,
/// in place of the built-in policy. Authentication, quota and proxy
/// refusals are never retried, whatever a policy says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Attempts in all, the first included (`1`: never retry).
    pub max_attempts: u32,
    /// Backoff before the first retry, doubled for each one after it.
    /// `None`: the built-in base. `RAPIDLM_RETRY_BASE_MS` overrides it.
    pub base_ms: Option<u64>,
    /// The longest wait. `None`: unbounded (the built-in policy). A provider
    /// asking for longer than this ends the retries.
    pub max_ms: Option<u64>,
    pub on: RetryClasses,
}

impl RetryPolicy {
    /// Today's policy: six attempts (five retries), every retry class, a 1 s
    /// base doubling each time, no longest wait.
    pub const fn builtin() -> Self {
        Self {
            max_attempts: 6,
            base_ms: None,
            max_ms: None,
            on: RetryClasses::ALL,
        }
    }

    /// The wait before retry `attempt + 1` (the first retry is `attempt` 0):
    /// the base (`env_base` over the policy's, over 1 s) doubled per retry,
    /// or a provider's retry-after when longer; capped at `max_ms`. `None`
    /// when the provider asks for longer than `max_ms`.
    pub fn wait_ms(
        &self,
        env_base: Option<u64>,
        attempt: u32,
        retry_after_ms: Option<u64>,
    ) -> Option<u64> {
        let base = env_base.or(self.base_ms).unwrap_or(1000);
        let doubling = base.saturating_mul(1u64 << attempt.min(16));
        let wait = retry_after_ms.map_or(doubling, |after| after.max(doubling));
        match self.max_ms {
            Some(max) if retry_after_ms.is_some_and(|after| after > max) => None,
            Some(max) => Some(wait.min(max)),
            None => Some(wait),
        }
    }
}

fn parse_retry(value: &toml::Value, prefix: &str) -> Result<RetryPolicy, UserConfigError> {
    let key = format!("{prefix}.retry");
    let table = value
        .as_table()
        .ok_or_else(|| UserConfigError::TypeMismatch { key: key.clone() })?;
    let invalid = |field: &str, reason: String| UserConfigError::InvalidValue {
        key: format!("{key}.{field}"),
        reason,
    };
    // A typo here would silently change a policy: unknown keys are errors.
    for field in table.keys() {
        if !matches!(field.as_str(), "max_attempts" | "base_ms" | "max_ms" | "on") {
            return Err(invalid(
                field,
                "not a retry key (max_attempts, base_ms, max_ms, on)".to_owned(),
            ));
        }
    }
    let int = |field: &str, min: i64, max: i64| -> Result<Option<i64>, UserConfigError> {
        match table.get(field) {
            None => Ok(None),
            Some(value) => {
                let number = value
                    .as_integer()
                    .ok_or_else(|| UserConfigError::TypeMismatch {
                        key: format!("{key}.{field}"),
                    })?;
                if number < min || number > max {
                    return Err(invalid(field, format!("must be {min}..={max}")));
                }
                Ok(Some(number))
            }
        }
    };
    let mut policy = RetryPolicy::builtin();
    if let Some(attempts) = int("max_attempts", 1, i64::from(MAX_RETRY_ATTEMPTS))? {
        policy.max_attempts = attempts as u32;
    }
    policy.base_ms = int("base_ms", 0, MAX_RETRY_WAIT_MS as i64)?.map(|ms| ms as u64);
    policy.max_ms = int("max_ms", 1, MAX_RETRY_WAIT_MS as i64)?.map(|ms| ms as u64);
    if let (Some(base), Some(max)) = (policy.base_ms, policy.max_ms)
        && base > max
    {
        return Err(invalid(
            "max_ms",
            "must not be less than base_ms".to_owned(),
        ));
    }
    if let Some(on) = table.get("on") {
        let names = on.as_array().ok_or_else(|| UserConfigError::TypeMismatch {
            key: format!("{key}.on"),
        })?;
        let mut classes = RetryClasses {
            rate_limit: false,
            server: false,
            network: false,
            rejected: false,
        };
        for name in names {
            let name = name.as_str().ok_or_else(|| UserConfigError::TypeMismatch {
                key: format!("{key}.on"),
            })?;
            match name {
                "rate_limit" => classes.rate_limit = true,
                "server" => classes.server = true,
                "network" => classes.network = true,
                "rejected" => classes.rejected = true,
                "auth" | "quota" | "proxy_auth" => {
                    return Err(invalid(
                        "on",
                        format!("{name} failures are never retried: asking again sends the same"),
                    ));
                }
                other => {
                    return Err(invalid(
                        "on",
                        format!(
                            "{other:?} is not a retry class (rate_limit, server, network, rejected)"
                        ),
                    ));
                }
            }
        }
        policy.on = classes;
    }
    Ok(policy)
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
    /// Provider-side model id sent on the wire (a hosted model's id, or a local
    /// server's model tag).
    pub model: String,
    /// Provider origin (http for local servers, https for TLS-verified
    /// remotes; e.g. `http://127.0.0.1:11434/v1`).
    pub base_url: String,
    /// Optional display name.
    pub name: Option<String>,
    /// Inline credential; wins over `env_key`.
    pub api_key: Option<String>,
    /// Env var names tried in order; first set, non-empty value wins.
    pub env_key: Vec<String>,
    /// OS keychain alias the key is stored under (`rapid setup --key-stdin`
    /// writes it); read when the model client is built, never before.
    pub keychain: Option<String>,
    /// `effort_ids = { high = "…", … }`: the model id sent at a given
    /// reasoning effort, in place of `model` (resolved when the client is
    /// built, after every effort floor).
    pub effort_ids: BTreeMap<ReasoningEffort, String>,
    /// `retry = { … }`: this model's step retry policy.
    pub retry: Option<RetryPolicy>,
    /// `continue_on_length = N`: when an answer ends for its output limit,
    /// up to `N` follow-on requests carry it forward into one message
    /// (`0`, the default: the answer ends where the limit cut it).
    pub continue_on_length: u32,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    /// Reasoning-effort request override; `None` means the provider default.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Per-model capability overrides (delivery goal §5): a model whose
    /// provider genuinely supports vision/prompt-caching/reasoning can turn
    /// them on instead of inheriting the historical conservative defaults.
    /// Absent = the documented default (off) — never guessed.
    pub vision: Option<bool>,
    pub caching: Option<bool>,
    pub reasoning: Option<bool>,
}

impl ModelEntry {
    /// The model id a request sends: the one `effort_ids` names for this
    /// entry's reasoning effort, else `model`. Records, prices and labels
    /// name this, so they name what went out.
    pub fn wire_model(&self) -> &str {
        self.reasoning_effort
            .and_then(|effort| self.effort_ids.get(&effort))
            .unwrap_or(&self.model)
    }
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
    /// The proxy model connections go through, resolved once from
    /// `[network] proxy` / `RAPIDLM_PROXY` and the proxy variables — the
    /// same for every model a run builds and for the setup probe. `None`:
    /// direct.
    pub proxy: Option<llm_router::providers::dial::ProxyConfig>,
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
    /// The OS keychain under this alias: `plaintext` stays `None` until the
    /// model client is built (invariant 11 — a handle until then).
    Keychain(String),
    Keyless,
}

/// Result of selection: a configured model, or the typed no-config fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelSelection {
    Configured {
        active: Box<ActiveModel>,
        warnings: Vec<String>,
    },
    Unconfigured {
        searched: Vec<String>,
    },
}

/// Typed configuration failure. Messages name keys/paths, never secret values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserConfigError {
    /// `RAPIDLM_CONFIG` points at a missing file.
    ExplicitConfigMissing {
        path: String,
    },
    Unreadable {
        path: String,
    },
    TooLarge {
        path: String,
    },
    InvalidUtf8 {
        path: String,
    },
    Syntax {
        path: String,
        message: String,
    },
    TypeMismatch {
        key: String,
    },
    InvalidValue {
        key: String,
        reason: String,
    },
    MissingKey {
        key: String,
    },
    NoModelsDefined,
    NoDefaultModel {
        available: Vec<String>,
    },
    UnknownDefaultModel {
        id: String,
        available: Vec<String>,
    },
    /// A `[phases]` override names a `[model.<id>]` that does not exist.
    UnknownPhaseModel {
        key: String,
        id: String,
        available: Vec<String>,
    },
}

impl std::error::Error for UserConfigError {}

impl UserConfig {
    /// Every defined `[model.<id>]` id, sorted (what `/model select` offers).
    pub fn model_ids(&self) -> Vec<String> {
        self.models.entries.keys().cloned().collect()
    }
}

/// The defined model ids for a process environment (what `/model list`
/// shows). Empty when no config resolves — a diagnostic, not an error.
pub fn list_configured_models(env: &[(String, String)]) -> Vec<String> {
    let source = resolve_config_source(env);
    let Some(config) = load_config(&source).ok().flatten() else {
        return Vec::new();
    };
    config.model_ids()
}

impl fmt::Display for UserConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitConfigMissing { path } => {
                write!(f, "RAPIDLM_CONFIG points at a missing config file: {path}")
            }
            Self::Unreadable { path } => write!(f, "config file is not readable: {path}"),
            Self::TooLarge { path } => {
                write!(
                    f,
                    "config file exceeds {MAX_USER_CONFIG_BYTES} bytes: {path}"
                )
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
    let shown = path.display().to_string();
    // Bound the read itself, not just check the size of what was already
    // fully buffered — the same stat-then-read gap `read_file_bounded`
    // closes elsewhere in this binary.
    let bytes = crate::exec_tools::read_file_bounded(path, MAX_USER_CONFIG_BYTES).map_err(
        |err| match err {
            crate::exec_tools::BoundedReadError::TooLarge => UserConfigError::TooLarge {
                path: shown.clone(),
            },
            crate::exec_tools::BoundedReadError::Io(_) => UserConfigError::Unreadable {
                path: shown.clone(),
            },
        },
    )?;
    let body = String::from_utf8(bytes).map_err(|_| UserConfigError::InvalidUtf8 {
        path: shown.clone(),
    })?;
    parse_config_document(&body, &shown)
}

/// Parse a config document body (pure).
pub fn parse_config_document(body: &str, path: &str) -> Result<UserConfig, UserConfigError> {
    let parsed: toml::Value = toml::from_str(body).map_err(|err| UserConfigError::Syntax {
        path: path.to_owned(),
        message: err.message().to_owned(),
    })?;
    let root = parsed.as_table().ok_or(UserConfigError::TypeMismatch {
        key: "(document root)".to_owned(),
    })?;

    let mut unknown_keys = Vec::new();
    for key in root.keys() {
        if key != "models" && key != "model" && key != "phases" && key != "network" {
            unknown_keys.push(key.clone());
        }
    }

    let mut models = ModelsSection::default();
    if let Some(section) = root.get("models") {
        let table = expect_table(section, "models")?;
        for key in table.keys() {
            if key != "default" && key != "fallback" {
                unknown_keys.push(format!("models.{key}"));
            }
        }
        if let Some(value) = table.get("default") {
            models.default = Some(expect_non_empty_str(value, "models.default")?.to_owned());
        }
        if let Some(value) = table.get("fallback") {
            let array = value.as_array().ok_or(UserConfigError::TypeMismatch {
                key: "models.fallback".to_owned(),
            })?;
            if array.len() > MAX_FALLBACK_MODELS {
                return Err(UserConfigError::InvalidValue {
                    key: "models.fallback".to_owned(),
                    reason: format!("at most {MAX_FALLBACK_MODELS} fallback entries"),
                });
            }
            for entry in array {
                models
                    .fallback
                    .push(expect_non_empty_str(entry, "models.fallback[]")?.to_owned());
            }
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
            models.entries.insert(
                id.clone(),
                parse_model_entry(id, entry_table, &mut unknown_keys)?,
            );
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
            phases
                .overrides
                .insert(purpose_name(purpose).to_owned(), id);
        }
    }

    let mut network = NetworkSection::default();
    if let Some(section) = root.get("network") {
        let table = expect_table(section, "network")?;
        for key in table.keys() {
            if key != "proxy" {
                unknown_keys.push(format!("network.{key}"));
            }
        }
        if let Some(value) = table.get("proxy") {
            let raw = value.as_str().ok_or(UserConfigError::TypeMismatch {
                key: "network.proxy".to_owned(),
            })?;
            network.proxy =
                Some(
                    ProxyMode::parse(raw).ok_or_else(|| UserConfigError::InvalidValue {
                        key: "network.proxy".to_owned(),
                        reason: "expected \"environment\" or \"none\"".to_owned(),
                    })?,
                );
        }
    }

    Ok(UserConfig {
        models,
        phases,
        network,
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
        "keychain",
        "effort_ids",
        "retry",
        "continue_on_length",
        "max_tokens",
        "context_window",
        "reasoning_effort",
        "vision",
        "caching",
        "reasoning",
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
        Some(toml::Value::String(raw)) => {
            vec![validate_env_name(raw, &format!("{prefix}.env_key"))?]
        }
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

    let keychain = match table.get("keychain") {
        None => None,
        Some(value) => {
            let alias = expect_non_empty_str(value, &format!("{prefix}.keychain"))?;
            auth::SecretRef::from_alias(alias).map_err(|_| UserConfigError::InvalidValue {
                key: format!("{prefix}.keychain"),
                reason: "a keychain alias is letters, digits and _ . : / - @, at most 128 bytes"
                    .to_owned(),
            })?;
            Some(alias.to_owned())
        }
    };

    let mut effort_ids = BTreeMap::new();
    if let Some(value) = table.get("effort_ids") {
        let ids = value
            .as_table()
            .ok_or_else(|| UserConfigError::TypeMismatch {
                key: format!("{prefix}.effort_ids"),
            })?;
        for (name, id) in ids {
            let effort =
                ReasoningEffort::parse(name).map_err(|_| UserConfigError::InvalidValue {
                    key: format!("{prefix}.effort_ids.{name}"),
                    reason: "expected none|minimal|low|medium|high|xhigh|ultra".to_owned(),
                })?;
            let id = expect_non_empty_str(id, &format!("{prefix}.effort_ids.{name}"))?;
            llm_router::ModelId::parse(id).map_err(|_| UserConfigError::InvalidValue {
                key: format!("{prefix}.effort_ids.{name}"),
                reason: "not a model id (alphanumerics with single - _ . / : separators)"
                    .to_owned(),
            })?;
            effort_ids.insert(effort, id.to_owned());
        }
    }
    let retry = match table.get("retry") {
        None => None,
        Some(value) => Some(parse_retry(value, &prefix)?),
    };
    let continue_on_length = match table.get("continue_on_length") {
        None => 0,
        Some(value) => {
            let key = format!("{prefix}.continue_on_length");
            let count = value
                .as_integer()
                .ok_or_else(|| UserConfigError::TypeMismatch { key: key.clone() })?;
            if !(0..=i64::from(MAX_CONTINUATIONS)).contains(&count) {
                return Err(UserConfigError::InvalidValue {
                    key,
                    reason: format!("must be 0..={MAX_CONTINUATIONS}"),
                });
            }
            count as u32
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
    let vision = parse_bool_field(table, &prefix, "vision")?;
    let caching = parse_bool_field(table, &prefix, "caching")?;
    let reasoning = parse_bool_field(table, &prefix, "reasoning")?;
    let reasoning_effort = match table.get("reasoning_effort") {
        None => None,
        Some(value) => {
            let raw = value.as_str().ok_or(UserConfigError::TypeMismatch {
                key: format!("{prefix}.reasoning_effort"),
            })?;
            Some(
                ReasoningEffort::parse(raw).map_err(|_| UserConfigError::InvalidValue {
                    key: format!("{prefix}.reasoning_effort"),
                    reason: "expected none|minimal|low|medium|high|xhigh|ultra".to_owned(),
                })?,
            )
        }
    };

    Ok(ModelEntry {
        provider,
        model,
        base_url,
        name,
        api_key,
        env_key,
        keychain,
        effort_ids,
        retry,
        continue_on_length,
        max_tokens,
        context_window,
        reasoning_effort,
        vision,
        caching,
        reasoning,
    })
}

/// Read one optional boolean `[model.<id>]` field. Unknown keys are already
/// rejected upstream; a wrong-typed value is a config error, never silence.
fn parse_bool_field(
    table: &toml::Table,
    prefix: &str,
    key: &str,
) -> Result<Option<bool>, UserConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| UserConfigError::InvalidValue {
                key: format!("{prefix}.{key}"),
                reason: "expected true or false".to_owned(),
            }),
    }
}

fn validate_env_name(raw: &str, key: &str) -> Result<String, UserConfigError> {
    let valid = !raw.is_empty()
        && raw.len() <= 256
        && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
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

fn expect_table<'a>(value: &'a toml::Value, key: &str) -> Result<&'a toml::Table, UserConfigError> {
    value.as_table().ok_or(UserConfigError::TypeMismatch {
        key: key.to_owned(),
    })
}

fn expect_non_empty_str<'a>(value: &'a toml::Value, key: &str) -> Result<&'a str, UserConfigError> {
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

/// Apply the precedence `RAPIDLM_MODEL` > `[models].default`, then
/// resolve the entry and its credential (`api_key` > first non-empty
/// `env_key` > keyless).
pub fn resolve_active(
    env: &[(String, String)],
    config: &UserConfig,
) -> Result<ActiveModel, UserConfigError> {
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
    let entry = config.models.entries.get(&default_id).ok_or_else(|| {
        UserConfigError::UnknownDefaultModel {
            id: default_id.clone(),
            available: available.clone(),
        }
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
            let profile =
                llm_router::ProfileId::parse(id).map_err(|_| UserConfigError::InvalidValue {
                    key: format!("phases.{purpose_name}"),
                    reason: "must satisfy the llm-router profile alphabet".to_owned(),
                })?;
            route = route.with_override(purpose, profile);
        }
    }
    Ok(ActiveModel {
        profile_id: default_id,
        entry: entry.clone(),
        credential: resolve_credential(entry, env),
        phase_route: route,
        proxy: resolve_proxy(env, config)?,
    })
}

/// Whether `key` is a proxy setting of this shell rather than of the config
/// file: `RAPIDLM_PROXY` or one of the proxy variables (any spelling —
/// Windows reads them ignoring case).
pub fn is_shell_proxy_setting(key: &str) -> bool {
    key == PROXY_MODE_ENV
        || ["https_proxy", "http_proxy", "no_proxy"]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
}

/// The proxy model connections go through: `RAPIDLM_PROXY` over
/// `[network] proxy`, default `none`. Under `environment` the proxy
/// variables are read from `env`; an unusable one is an error naming the
/// variable (never its value), and none set means direct.
pub fn resolve_proxy(
    env: &[(String, String)],
    config: &UserConfig,
) -> Result<Option<llm_router::providers::dial::ProxyConfig>, UserConfigError> {
    let (mode, origin) = match env_value(env, PROXY_MODE_ENV) {
        Some(raw) => (
            ProxyMode::parse(raw).ok_or_else(|| UserConfigError::InvalidValue {
                key: PROXY_MODE_ENV.to_owned(),
                reason: "expected \"environment\" or \"none\"".to_owned(),
            })?,
            format!("{PROXY_MODE_ENV}=environment"),
        ),
        None => (
            config.network.proxy.unwrap_or(ProxyMode::None),
            "network.proxy = \"environment\"".to_owned(),
        ),
    };
    if mode == ProxyMode::None {
        return Ok(None);
    }
    let proxy = llm_router::providers::dial::ProxyConfig::from_env(env).map_err(|err| {
        UserConfigError::InvalidValue {
            key: origin,
            reason: err.to_string(),
        }
    })?;
    Ok((!proxy.is_empty()).then_some(proxy))
}

/// Resolve `[models] fallback` into an ordered list of `ActiveModel`s, in
/// the user's configured order. An id that doesn't name a defined
/// `[model.<id>]` entry is skipped with a warning rather than failing
/// startup — a fallback-list typo must never block the primary model from
/// working, only weaken the fallback itself. The primary (`active`) is
/// never duplicated into this list even if the user names it again.
pub fn resolve_fallback_chain(
    env: &[(String, String)],
    config: &UserConfig,
    active: &ActiveModel,
) -> (Vec<ActiveModel>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut warnings = Vec::new();
    for id in &config.models.fallback {
        if id == &active.profile_id {
            continue;
        }
        let Some(entry) = config.models.entries.get(id) else {
            warnings.push(format!(
                "models.fallback names '{id}', which has no [model.{id}] entry; skipped"
            ));
            continue;
        };
        let Ok(profile) = llm_router::ProfileId::parse(id) else {
            warnings.push(format!(
                "models.fallback entry '{id}' does not satisfy the llm-router profile alphabet; skipped"
            ));
            continue;
        };
        resolved.push(ActiveModel {
            profile_id: id.clone(),
            entry: entry.clone(),
            credential: resolve_credential(entry, env),
            phase_route: PhaseRoute::new(profile),
            proxy: active.proxy.clone(),
        });
    }
    (resolved, warnings)
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
    resolve_purpose_model_for(active, env, config, purpose)
}

/// [`resolve_purpose_model`] for a primary already resolved — the gated one
/// a run uses (a managed lock may have overruled the shell's override, which
/// must not be re-read here).
pub fn resolve_purpose_model_for(
    active: ActiveModel,
    env: &[(String, String)],
    config: &UserConfig,
    purpose: llm_router::provider::ModelPurpose,
) -> Result<ActiveModel, UserConfigError> {
    let routed = active.phase_route.route(purpose).as_str();
    if routed == active.profile_id {
        return Ok(active);
    }
    let entry =
        config
            .models
            .entries
            .get(routed)
            .ok_or_else(|| UserConfigError::UnknownPhaseModel {
                key: format!("phases.{}", llm_router::purpose_name(purpose)),
                id: routed.to_owned(),
                available: config.models.entries.keys().cloned().collect(),
            })?;
    Ok(ActiveModel {
        profile_id: routed.to_owned(),
        entry: entry.clone(),
        credential: resolve_credential(entry, env),
        phase_route: active.phase_route,
        proxy: active.proxy,
    })
}

/// Credential precedence: inline `api_key`, then the first set,
/// non-empty `env_key` entry, then the `keychain` alias (read when the
/// client is built), then keyless.
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
    if let Some(alias) = &entry.keychain {
        return ResolvedCredential {
            plaintext: None,
            source: CredentialSource::Keychain(alias.clone()),
        };
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
    select_active_model_with_override(env, None)
}

/// [`Self::select_active_model_gated`] with an explicit model-id override —
/// the mid-session `/model select <id>` path. The override has the same
/// precedence as `RAPIDLM_MODEL` (above `[models].default`) and must name a
/// defined model; an unknown id fails with the defined list so the
/// composer can show the choices.
pub fn select_active_model_with_override(
    env: &[(String, String)],
    override_id: Option<&str>,
) -> Result<ModelSelection, crate::managed_config::GatedConfigError> {
    let source = resolve_config_source(env);
    let searched = match &source {
        ConfigSource::ExplicitPath(path) | ConfigSource::HomeFallback(path) => {
            vec![path.display().to_string()]
        }
    };
    let Some(mut config) = load_config(&source)? else {
        return Ok(ModelSelection::Unconfigured { searched });
    };
    // The session override has RAPIDLM_MODEL's precedence (above
    // `[models].default`) but sits under a managed lock, exactly like the
    // env var does. Implemented by making it the env-less default: filter
    // RAPIDLM_MODEL out of the env the resolver sees and set the cloned
    // config's default, so `[phases]` routes and warnings stay consistent.
    let mut effective_env: Vec<(String, String)> = env.to_vec();
    let policy = crate::managed_config::load_policy(env)?;
    let locked = policy
        .as_ref()
        .is_some_and(|policy| policy.locked_default().is_some());
    if let Some(id) = override_id {
        // Under a lock the choice is overruled (and reported as such by the
        // gate), even when it names a profile removed since `/model select`.
        if !locked && !config.model_ids().contains(&id.to_owned()) {
            return Err(crate::managed_config::GatedConfigError::Config(
                UserConfigError::UnknownDefaultModel {
                    id: id.to_owned(),
                    available: config.model_ids(),
                },
            ));
        }
        effective_env.retain(|(key, _)| key != DEFAULT_MODEL_ENV);
        config.models.default = Some(id.to_owned());
    }
    let mut warnings = config
        .unknown_keys
        .iter()
        .map(|key| format!("unknown config key '{key}'"))
        .collect::<Vec<_>>();
    let gated = crate::managed_config::resolve_gated(&effective_env, &config, policy.as_ref())?;
    for report in &gated.reports {
        warnings.push(format!("managed gate: {report}"));
    }
    Ok(ModelSelection::Configured {
        active: Box::new(gated.active),
        warnings,
    })
}

/// Process-env entry point for the managed-policy-aware selection.
pub fn select_from_process_env_gated()
-> Result<ModelSelection, crate::managed_config::GatedConfigError> {
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
    use std::fs;

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
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"
name = "Local server"
env_key = "LOCAL_API_KEY"
max_tokens = 2048
context_window = 65536

[model.cloud]
provider = "anthropic"
model = "remote-large"
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
        assert_eq!(local.model, "local-small");
        assert_eq!(local.env_key, vec!["LOCAL_API_KEY".to_owned()]);
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
        assert_eq!(
            config.unknown_keys,
            vec!["models.typo_key", "model.a.env_ky"]
        );
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
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"
reasoning_effort = "high"

[model.cloud]
provider = "anthropic"
model = "remote-large"
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
        let config = parse_config_document(PHASES_DOC, "test.toml").expect("parse");
        let active = resolve_active(&[], &config).expect("resolve");
        assert_eq!(active.profile_id, "local");
        assert_eq!(
            active
                .phase_route
                .route(llm_router::provider::ModelPurpose::Compact)
                .as_str(),
            "cloud"
        );
        assert_eq!(
            active
                .phase_route
                .route(llm_router::provider::ModelPurpose::Chat)
                .as_str(),
            "local"
        );
        // The default-model env override also re-roots the phase route.
        let env = env(&[("RAPIDLM_MODEL", "cloud")]);
        let active = resolve_active(&env, &config).expect("resolve override");
        assert_eq!(
            active
                .phase_route
                .route(llm_router::provider::ModelPurpose::Chat)
                .as_str(),
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
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        let err = resolve_active(&[], &config).expect_err("unknown phase target");
        assert!(
            err.to_string()
                .contains("phases.compact names model 'missing'")
        );
    }

    #[test]
    fn parses_and_resolves_the_fallback_chain_in_order() {
        let doc = r#"
[models]
default = "local"
fallback = ["cloud", "cloud2"]

[model.local]
provider = "openai-compatible"
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"

[model.cloud]
provider = "anthropic"
model = "remote-large"
base_url = "http://gateway.internal:8080"
api_key = "inline-secret"

[model.cloud2]
provider = "openai-compatible"
model = "other"
base_url = "http://127.0.0.1:9999/v1"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        assert_eq!(config.models.fallback, vec!["cloud", "cloud2"]);
        let active = resolve_active(&[], &config).expect("resolve");
        let (chain, warnings) = resolve_fallback_chain(&[], &config, &active);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].profile_id, "cloud");
        assert_eq!(chain[1].profile_id, "cloud2");
    }

    #[test]
    fn fallback_chain_skips_an_unknown_id_with_a_warning_not_an_error() {
        let doc = r#"
[models]
default = "local"
fallback = ["typo-id"]

[model.local]
provider = "openai-compatible"
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        let active = resolve_active(&[], &config).expect("resolve");
        let (chain, warnings) = resolve_fallback_chain(&[], &config, &active);
        assert!(chain.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("typo-id"));
    }

    #[test]
    fn fallback_chain_never_duplicates_the_primary() {
        let doc = r#"
[models]
default = "local"
fallback = ["local", "cloud"]

[model.local]
provider = "openai-compatible"
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"

[model.cloud]
provider = "anthropic"
model = "remote-large"
base_url = "http://gateway.internal:8080"
api_key = "inline-secret"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        let active = resolve_active(&[], &config).expect("resolve");
        let (chain, warnings) = resolve_fallback_chain(&[], &config, &active);
        assert!(warnings.is_empty());
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].profile_id, "cloud");
    }

    #[test]
    fn no_fallback_configured_resolves_to_an_empty_chain() {
        let doc = r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"
"#;
        let config = parse_config_document(doc, "test.toml").expect("parse");
        assert!(config.models.fallback.is_empty());
        let active = resolve_active(&[], &config).expect("resolve");
        let (chain, warnings) = resolve_fallback_chain(&[], &config, &active);
        assert!(chain.is_empty());
        assert!(warnings.is_empty());
    }

    /// Two-model document with a compact-phase override to the cloud entry.
    const PHASES_DOC: &str = r#"
[models]
default = "local"

[phases]
compact = "cloud"

[model.local]
provider = "openai-compatible"
model = "local-small"
base_url = "http://127.0.0.1:11434/v1"

[model.cloud]
provider = "anthropic"
model = "remote-large"
base_url = "http://gateway.internal:8080"
api_key = "inline-secret"
"#;

    #[test]
    fn parse_rejects_unknown_provider_and_bad_scalars() {
        let doc = r#"
[model.a]
provider = "some-other-dialect"
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
        let resolved = resolve_credential(entry, &env(&[("B_KEY", "b"), ("LOCAL_API_KEY", "k")]));
        assert_eq!(
            resolved.source,
            CredentialSource::EnvVar("LOCAL_API_KEY".to_owned())
        );
        // Empty values are skipped, not selected.
        let resolved = resolve_credential(entry, &env(&[("LOCAL_API_KEY", "")]));
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
        assert!(
            matches!(err, UserConfigError::InvalidValue { key, .. } if key == DEFAULT_MODEL_ENV)
        );
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
    fn read_config_file_bounds_the_read_and_still_reports_too_large() {
        // `read_config_file` used to buffer the whole file via `fs::read`
        // before checking its length; it now reads through `read_file_
        // bounded`'s `max_bytes + 1` cap instead. The `TooLarge` outcome
        // for an oversized document is unchanged either way (this pins
        // that observable contract down against a regression in the
        // rewiring); the actual memory-bounding property of the read
        // itself is `read_file_bounded`'s own, already-tested guarantee
        // (see `exec_tools.rs`/`host.rs`'s dedicated tests for that).
        let dir = std::env::temp_dir().join(format!("rapidlm-cfg-bound-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        let padding = "a".repeat(MAX_USER_CONFIG_BYTES * 2);
        let doc = format!("{VALID_DOC}\npadding = \"{padding}\"\n");
        assert!(
            toml::from_str::<toml::Value>(&doc).is_ok(),
            "the fixture itself must be valid TOML when read in full"
        );
        let file = dir.join("oversized.toml");
        fs::write(&file, &doc).expect("write");

        let err = read_config_file(&file).expect_err("oversized document must be rejected");
        assert!(matches!(err, UserConfigError::TooLarge { .. }), "{err:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn home_fallback_missing_yields_unconfigured() {
        let dir = std::env::temp_dir().join(format!("rapidlm-empty-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        let selection =
            select_active_model(&env(&[(HOME_ENV, dir.to_str().unwrap())])).expect("selection");
        match selection {
            ModelSelection::Unconfigured { searched } => {
                assert_eq!(searched.len(), 1);
                assert!(
                    Path::new(&searched[0]).ends_with(Path::new(".rapidlm").join("config.toml")),
                    "{}",
                    searched[0]
                );
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

    #[test]
    fn a_keychain_alias_is_a_credential_source_after_the_inline_key_and_the_variables() {
        let entry = |extra: &str| {
            let doc = format!(
                "[models]\ndefault = \"p\"\n\n[model.p]\nprovider = \"openai-compatible\"\n\
model = \"m\"\nbase_url = \"http://127.0.0.1:1/v1\"\n{extra}"
            );
            parse_config_document(&doc, "c").map(|config| config.models.entries["p"].clone())
        };
        let kept = entry("keychain = \"rapidlm-model-p\"\nenv_key = \"P_KEY\"\n").expect("parses");
        assert_eq!(kept.keychain.as_deref(), Some("rapidlm-model-p"));
        // A set variable wins; unset, the keychain alias is the source.
        assert_eq!(
            resolve_credential(&kept, &env(&[("P_KEY", "sk-env")])).source,
            CredentialSource::EnvVar("P_KEY".to_owned())
        );
        let resolved = resolve_credential(&kept, &[]);
        assert_eq!(
            resolved.source,
            CredentialSource::Keychain("rapidlm-model-p".to_owned())
        );
        assert_eq!(resolved.plaintext, None);
        let inline =
            entry("keychain = \"rapidlm-model-p\"\napi_key = \"sk-inline\"\n").expect("parses");
        assert_eq!(
            resolve_credential(&inline, &[]).source,
            CredentialSource::InlineApiKey
        );
        for bad in [
            "keychain = \"has space\"",
            "keychain = \"\"",
            "keychain = 3",
        ] {
            assert!(entry(&format!("{bad}\n")).is_err(), "{bad}");
        }
        assert!(
            parse_config_document(
                "[model.p]\nprovider = \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"http://127.0.0.1:1/v1\"\nkeychain = \"a\"\n",
                "c"
            )
            .expect("parses")
            .unknown_keys
            .is_empty()
        );
    }

    #[test]
    fn effort_ids_and_a_retry_table_are_read_and_checked() {
        let entry = |extra: &str| {
            let doc = format!(
                "[models]\ndefault = \"p\"\n\n[model.p]\nprovider = \"openai-compatible\"\n\
model = \"m\"\nbase_url = \"http://127.0.0.1:1/v1\"\n{extra}"
            );
            parse_config_document(&doc, "c").map(|config| config.models.entries["p"].clone())
        };
        let parsed = entry(
            "effort_ids = { high = \"m-think\", low = \"m-fast\" }\n\
             retry = { max_attempts = 3, base_ms = 200, max_ms = 5000, on = [\"rate_limit\", \"network\"] }\n",
        )
        .expect("parses");
        assert_eq!(
            parsed
                .effort_ids
                .get(&ReasoningEffort::High)
                .map(String::as_str),
            Some("m-think")
        );
        assert_eq!(
            parsed
                .effort_ids
                .get(&ReasoningEffort::Low)
                .map(String::as_str),
            Some("m-fast")
        );
        let retry = parsed.retry.expect("retry");
        assert_eq!(
            (retry.max_attempts, retry.base_ms, retry.max_ms),
            (3, Some(200), Some(5000))
        );
        assert!(retry.on.allows(RetryClass::RateLimit) && retry.on.allows(RetryClass::Network));
        assert!(!retry.on.allows(RetryClass::Server) && !retry.on.allows(RetryClass::Rejected));
        // Omitted keys keep the built-in values.
        let partial = entry("retry = { max_attempts = 1 }\n").expect("parses");
        assert_eq!(
            partial.retry,
            Some(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::builtin()
            })
        );
        for (bad, why) in [
            ("retry = { on = [\"auth\"] }", "never retried"),
            ("retry = { on = [\"quota\"] }", "never retried"),
            ("retry = { on = [\"everything\"] }", "not a retry class"),
            ("retry = { max_attempts = 0 }", "1..="),
            ("retry = { max_attempts = 12 }", "1..="),
            (
                "retry = { base_ms = 900, max_ms = 100 }",
                "less than base_ms",
            ),
            ("retry = { attempts = 3 }", "not a retry key"),
            ("effort_ids = { turbo = \"m\" }", "none|minimal"),
            ("effort_ids = { high = \"has space\" }", "not a model id"),
        ] {
            let err = entry(&format!("{bad}\n")).expect_err(bad);
            assert!(err.to_string().contains(why), "{bad}: {err}");
        }
        assert!(entry("retry = 3\n").is_err());
    }

    #[test]
    fn the_wire_model_is_the_one_named_for_the_effort() {
        let doc = "[model.p]\nprovider = \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
reasoning_effort = \"high\"\neffort_ids = { high = \"m-think\" }\n";
        let mut entry = parse_config_document(doc, "c")
            .expect("parses")
            .models
            .entries["p"]
            .clone();
        assert_eq!(entry.wire_model(), "m-think");
        entry.reasoning_effort = Some(ReasoningEffort::Low);
        assert_eq!(entry.wire_model(), "m");
        entry.reasoning_effort = None;
        assert_eq!(entry.wire_model(), "m");
    }

    const PROXIED_DOC: &str = r#"
[models]
default = "local"
fallback = ["cloud"]

[network]
proxy = "environment"

[model.local]
provider = "openai-compatible"
model = "local-small"
base_url = "http://model.example.test:8080/v1"

[model.cloud]
provider = "openai-compatible"
model = "cloud-large"
base_url = "https://cloud.example.test/v1"

[phases]
compact = "cloud"
"#;

    #[test]
    fn the_proxy_is_opt_in_and_every_model_of_a_run_shares_it() {
        let proxy_vars = [("http_proxy", "http://proxy.example.test:3128")];
        // S11: an exported proxy variable changes nothing unasked.
        let direct = parse_config_document(VALID_DOC, "c").expect("parses");
        assert_eq!(direct.network.proxy, None);
        let active = resolve_active(&env(&proxy_vars), &direct).expect("active");
        assert_eq!(active.proxy, None);

        let config = parse_config_document(PROXIED_DOC, "c").expect("parses");
        assert_eq!(config.network.proxy, Some(ProxyMode::Environment));
        assert!(config.unknown_keys.is_empty(), "{:?}", config.unknown_keys);
        let active = resolve_active(&env(&proxy_vars), &config).expect("active");
        let proxy = active
            .proxy
            .clone()
            .expect("the opt-in reads the variables");
        let via = proxy
            .for_target(false, "model.example.test", 8080)
            .expect("proxied");
        assert_eq!((via.host(), via.port()), ("proxy.example.test", 3128));
        // The fallback chain and the phase models go the same way.
        let (chain, _) = resolve_fallback_chain(&env(&proxy_vars), &config, &active);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].proxy, active.proxy);
        let compact = resolve_purpose_model_for(
            active.clone(),
            &env(&proxy_vars),
            &config,
            llm_router::provider::ModelPurpose::Compact,
        )
        .expect("compact");
        assert_eq!(compact.profile_id, "cloud");
        assert_eq!(compact.proxy, active.proxy);

        // Opted in with no variable set: direct.
        assert_eq!(resolve_active(&[], &config).expect("active").proxy, None);
        // RAPIDLM_PROXY wins over the file, both ways.
        let mut off = env(&proxy_vars);
        off.push((PROXY_MODE_ENV.to_owned(), "none".to_owned()));
        assert_eq!(resolve_active(&off, &config).expect("active").proxy, None);
        let mut on = env(&proxy_vars);
        on.push((PROXY_MODE_ENV.to_owned(), "environment".to_owned()));
        assert!(
            resolve_active(&on, &direct)
                .expect("active")
                .proxy
                .is_some()
        );
    }

    #[test]
    fn an_unusable_proxy_setting_is_an_error_naming_it_never_its_value() {
        let config = parse_config_document(PROXIED_DOC, "c").expect("parses");
        let err = resolve_active(
            &env(&[("HTTPS_PROXY", "https://user:hunter2@proxy.example.test")]),
            &config,
        )
        .expect_err("an https:// proxy is refused");
        let shown = err.to_string();
        assert!(shown.contains("network.proxy"), "{shown}");
        assert!(shown.contains("HTTPS_PROXY"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
        // Not opted in, the same variable is never read.
        let direct = parse_config_document(VALID_DOC, "c").expect("parses");
        assert!(
            resolve_active(
                &env(&[("HTTPS_PROXY", "https://user:hunter2@proxy.example.test")]),
                &direct
            )
            .is_ok()
        );
        let err = resolve_active(&env(&[(PROXY_MODE_ENV, "on")]), &direct).expect_err("not a mode");
        assert!(err.to_string().contains(PROXY_MODE_ENV), "{err}");
        for (doc, key) in [
            ("[network]\nproxy = \"system\"\n", "network.proxy"),
            ("[network]\nproxy = true\n", "network.proxy"),
        ] {
            let err = parse_config_document(doc, "c").expect_err(doc);
            assert!(err.to_string().contains(key), "{err}");
        }
        let odd = parse_config_document("[network]\nproxy = \"none\"\ntimeout = 3\n", "c")
            .expect("parses");
        assert_eq!(odd.network.proxy, Some(ProxyMode::None));
        assert_eq!(odd.unknown_keys, vec!["network.timeout".to_owned()]);
    }
}

#[cfg(test)]
mod capability_override_tests {
    use super::*;

    fn entry_from(toml_text: &str) -> ModelEntry {
        let table: toml::Table = toml::from_str(toml_text)
            .map_err(|err| UserConfigError::InvalidValue {
                key: "toml".to_owned(),
                reason: err.to_string(),
            })
            .unwrap();
        parse_model_entry("m", &table, &mut Vec::new()).unwrap()
    }

    #[test]
    fn vision_caching_reasoning_overrides_parse_and_default_off() {
        let enabled = entry_from(
            r#"
            provider = "openai-compatible"
            model = "test-model"
            base_url = "http://127.0.0.1:9/v1"
            api_key = "k"
            vision = true
            caching = true
            reasoning = true
            "#,
        );
        assert_eq!(enabled.vision, Some(true));
        assert_eq!(enabled.caching, Some(true));
        assert_eq!(enabled.reasoning, Some(true));

        let plain = entry_from(
            r#"
            provider = "openai-compatible"
            model = "test-model"
            base_url = "http://127.0.0.1:9/v1"
            api_key = "k"
            "#,
        );
        assert_eq!(plain.vision, None);
        assert_eq!(plain.caching, None);
        assert_eq!(plain.reasoning, None);

        // Wrong-typed value is a config error, never silence.
        let broken: toml::Table = toml::from_str(
            r#"
            provider = "openai-compatible"
            model = "test-model"
            base_url = "http://127.0.0.1:9/v1"
            api_key = "k"
            vision = "yes"
            "#,
        )
        .unwrap();
        assert!(parse_model_entry("m", &broken, &mut Vec::new()).is_err());
    }

    #[test]
    fn under_a_lock_a_stale_session_override_is_overruled_not_fatal() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-stale-override-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[models]\ndefault = \"corp\"\n\n[model.corp]\nprovider = \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n\n[model.other]\nprovider = \"openai-compatible\"\nmodel = \"n\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n",
        )
        .expect("config");
        let policy = dir.join("policy.toml");
        std::fs::write(
            &policy,
            format!(
                "schema = \"{}\"\n[policy]\nlocked_default = \"corp\"\n",
                crate::managed_config::MANAGED_SCHEMA
            ),
        )
        .expect("policy");
        let env = vec![(CONFIG_PATH_ENV.to_owned(), config.display().to_string())];
        // Without a lock, a removed profile is the error it always was.
        assert!(select_active_model_with_override(&env, Some("removed")).is_err());
        let mut locked = env.clone();
        locked.push((
            crate::managed_config::MANAGED_CONFIG_ENV.to_owned(),
            policy.display().to_string(),
        ));
        // A stale choice and a valid one alike: overruled, and said so.
        for choice in ["removed", "other"] {
            match select_active_model_with_override(&locked, Some(choice))
                .expect("the lock decides")
            {
                ModelSelection::Configured { active, warnings } => {
                    assert_eq!(active.profile_id, "corp", "{choice}");
                    assert!(
                        warnings
                            .iter()
                            .any(|warning| warning.contains("managed gate")
                                && warning.contains("locked")),
                        "{choice}: {warnings:?}"
                    );
                }
                other => panic!("configured, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
