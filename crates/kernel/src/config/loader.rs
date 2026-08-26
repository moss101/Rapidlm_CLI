//! Deterministic configuration precedence loader.
//!
//! Ordinary settings merge as `CLI > env > user > workspace > defaults`.
//! Security-policy intersection is a later crate and is not applied here.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::{ConfigError, RapidConfig};
use serde_json::{Map, Value};

/// Environment variable prefix recognized by [`config_key_from_env_name`].
pub const ENV_PREFIX: &str = "RAPIDLM_";

/// Maximum UTF-8 bytes accepted for a user or workspace document.
pub const MAX_CONFIG_DOCUMENT_BYTES: usize = 256 * 1024;

/// Maximum env or CLI override entries accepted from one source.
pub const MAX_OVERRIDE_ENTRIES: usize = 256;

/// Maximum UTF-8 bytes accepted in an override key.
pub const MAX_OVERRIDE_KEY_BYTES: usize = 256;

/// Maximum UTF-8 bytes accepted in an override value.
pub const MAX_OVERRIDE_VALUE_BYTES: usize = 8 * 1024;

const MAX_UNKNOWN_KEYS: usize = 64;

/// Cooperative cancellation for [`load_config`].
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Layer that supplied a field's effective value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ConfigOrigin {
    Defaults,
    Workspace,
    User,
    Environment,
    Cli,
}

/// User or workspace TOML document. Debug omits the body.
#[derive(Clone, Eq, PartialEq)]
pub struct ConfigText {
    name: String,
    body: String,
}

/// Single env or CLI override. Debug omits the value.
#[derive(Clone, Eq, PartialEq)]
pub struct ConfigOverride {
    key: String,
    value: String,
}

/// Inputs for [`load_config`]. The loader does not read the filesystem or process env.
pub struct ConfigSources {
    pub workspace: Option<ConfigText>,
    pub user: Option<ConfigText>,
    pub env: Vec<ConfigOverride>,
    pub cli: Vec<ConfigOverride>,
    pub cancel: CancellationToken,
}

/// Merged configuration plus per-field origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigLoadResult {
    pub config: RapidConfig,
    pub provenance: BTreeMap<String, ConfigOrigin>,
    pub unknown_keys: Vec<UnknownConfigKey>,
}

/// Unknown key observed in a document. The value is never retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownConfigKey {
    pub source: ConfigOrigin,
    pub key: String,
}

/// Typed load failure. Messages name source and key and never include values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigLoadError {
    Cancelled,
    SourceTooLarge { source: ConfigOrigin },
    TooManyOverrides { source: ConfigOrigin },
    InvalidSyntax { source: ConfigOrigin },
    TypeMismatch { source: ConfigOrigin, key: String },
    InvalidValue { source: ConfigOrigin, key: String },
    UnsupportedSchema { source: ConfigOrigin, found: u16 },
    UnknownOverride { source: ConfigOrigin, key: String },
    InternalDefaults,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), ConfigLoadError> {
        if self.is_cancelled() {
            Err(ConfigLoadError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Defaults => "defaults",
            Self::Workspace => "workspace",
            Self::User => "user",
            Self::Environment => "environment",
            Self::Cli => "cli",
        }
    }
}

impl fmt::Display for ConfigOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ConfigText {
    pub fn new(name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            body: body.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> usize {
        self.body.len()
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }
}

impl fmt::Debug for ConfigText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigText")
            .field("name", &self.name)
            .field("bytes", &self.body.len())
            .finish()
    }
}

impl ConfigOverride {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

impl fmt::Debug for ConfigOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigOverride")
            .field("key", &self.key)
            .finish()
    }
}

impl ConfigSources {
    pub fn empty() -> Self {
        Self {
            workspace: None,
            user: None,
            env: Vec::new(),
            cli: Vec::new(),
            cancel: CancellationToken::new(),
        }
    }
}

impl Default for ConfigSources {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for ConfigSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigSources")
            .field("workspace", &self.workspace)
            .field("user", &self.user)
            .field("env", &self.env)
            .field("cli", &self.cli)
            .field("cancel_cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

impl ConfigLoadResult {
    pub fn origin(&self, key: &str) -> Option<ConfigOrigin> {
        self.provenance.get(key).copied()
    }
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("configuration load cancelled"),
            Self::SourceTooLarge { source } => {
                write!(
                    f,
                    "configuration source {source} exceeds {MAX_CONFIG_DOCUMENT_BYTES} bytes"
                )
            }
            Self::TooManyOverrides { source } => {
                write!(f, "too many {source} overrides")
            }
            Self::InvalidSyntax { source } => {
                write!(f, "invalid configuration syntax in {source}")
            }
            Self::TypeMismatch { source, key } => {
                write!(f, "invalid type for {source}:{key}")
            }
            Self::InvalidValue { source, key } => {
                write!(f, "invalid value for {source}:{key}")
            }
            Self::UnsupportedSchema { source, found } => {
                write!(f, "unsupported configuration schema {found} in {source}")
            }
            Self::UnknownOverride { source, key } => {
                write!(f, "unknown configuration key {source}:{key}")
            }
            Self::InternalDefaults => f.write_str("failed to encode documented defaults"),
        }
    }
}

impl Error for ConfigLoadError {}

/// Map `RAPIDLM_*` names onto dotted schema paths.
///
/// Underscores are interpreted as either path separators or name characters
/// so `RAPIDLM_AGENTS_MAX_PARALLEL` becomes `agents.max_parallel`.
pub fn config_key_from_env_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix(ENV_PREFIX)?;
    if rest.is_empty() {
        return None;
    }
    resolve_known_path(&rest.to_ascii_lowercase())
}

/// Merge defaults, workspace, user, environment, and CLI with deterministic precedence.
pub fn load_config(sources: &ConfigSources) -> Result<ConfigLoadResult, ConfigLoadError> {
    sources.cancel.check()?;

    let mut root = default_object()?;
    let mut provenance = BTreeMap::new();
    record_leaves(&root, "", ConfigOrigin::Defaults, &mut provenance);
    let mut unknown_keys = Vec::new();

    apply_document(
        &mut root,
        &mut provenance,
        &mut unknown_keys,
        sources.workspace.as_ref(),
        ConfigOrigin::Workspace,
        &sources.cancel,
    )?;
    apply_document(
        &mut root,
        &mut provenance,
        &mut unknown_keys,
        sources.user.as_ref(),
        ConfigOrigin::User,
        &sources.cancel,
    )?;
    apply_overrides(
        &mut root,
        &mut provenance,
        &sources.env,
        ConfigOrigin::Environment,
        true,
        &sources.cancel,
    )?;
    apply_overrides(
        &mut root,
        &mut provenance,
        &sources.cli,
        ConfigOrigin::Cli,
        false,
        &sources.cancel,
    )?;

    sources.cancel.check()?;
    let document = RapidConfig::from_json_value(Value::Object(root)).map_err(map_internal)?;
    Ok(ConfigLoadResult {
        config: document.config,
        provenance,
        unknown_keys,
    })
}

fn default_object() -> Result<Map<String, Value>, ConfigLoadError> {
    match serde_json::to_value(RapidConfig::default()) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(ConfigLoadError::InternalDefaults),
    }
}

fn apply_document(
    root: &mut Map<String, Value>,
    provenance: &mut BTreeMap<String, ConfigOrigin>,
    unknown_keys: &mut Vec<UnknownConfigKey>,
    document: Option<&ConfigText>,
    source: ConfigOrigin,
    cancel: &CancellationToken,
) -> Result<(), ConfigLoadError> {
    cancel.check()?;
    let Some(document) = document else {
        return Ok(());
    };
    if document.body.len() > MAX_CONFIG_DOCUMENT_BYTES {
        return Err(ConfigLoadError::SourceTooLarge { source });
    }
    let parsed = parse_toml_table(source, &document.body)?;
    overlay_map(root, &parsed, "", source, provenance, unknown_keys, cancel)?;
    validate_layer(root, source)
}

fn apply_overrides(
    root: &mut Map<String, Value>,
    provenance: &mut BTreeMap<String, ConfigOrigin>,
    overrides: &[ConfigOverride],
    source: ConfigOrigin,
    allow_env_names: bool,
    cancel: &CancellationToken,
) -> Result<(), ConfigLoadError> {
    cancel.check()?;
    if overrides.len() > MAX_OVERRIDE_ENTRIES {
        return Err(ConfigLoadError::TooManyOverrides { source });
    }
    for item in overrides {
        cancel.check()?;
        if item.key.len() > MAX_OVERRIDE_KEY_BYTES || item.value.len() > MAX_OVERRIDE_VALUE_BYTES {
            return Err(ConfigLoadError::SourceTooLarge { source });
        }
        if item.key.is_empty() {
            return Err(ConfigLoadError::UnknownOverride {
                source,
                key: String::new(),
            });
        }
        let path = resolve_override_key(&item.key, allow_env_names).ok_or_else(|| {
            ConfigLoadError::UnknownOverride {
                source,
                key: item.key.clone(),
            }
        })?;
        let value = parse_override_literal(&item.value);
        set_leaf(root, &path, value, source)?;
        provenance.insert(path, source);
    }
    if overrides.is_empty() {
        Ok(())
    } else {
        validate_layer(root, source)
    }
}

fn resolve_override_key(raw: &str, allow_env_names: bool) -> Option<String> {
    if RapidConfig::is_known_key_path(raw) && is_assignable_leaf(raw) {
        return Some(raw.to_owned());
    }
    if allow_env_names
        && let Some(path) = config_key_from_env_name(raw)
        && is_assignable_leaf(&path)
    {
        return Some(path);
    }
    None
}

fn is_assignable_leaf(path: &str) -> bool {
    !path.is_empty() && RapidConfig::is_known_key_path(path) && !has_known_child(path)
}

fn has_known_child(path: &str) -> bool {
    // Tables are known paths and also have children. Leaves do not.
    // Probe a conservative child name that exists on every table we ship.
    const PROBES: &[&str] = &[
        "default_policy",
        "embeddings",
        "max_index_bytes",
        "max_parallel",
        "max_write_parallel",
        "pool",
        "managed",
        "enabled",
        "max_background",
        "background_default_role",
        "mailbox_max_messages",
        "mailbox_max_inline_bytes",
        "clean_context",
        "max_child_context_tokens",
        "default_tier",
        "network",
        "mode",
        "content",
        "require_signed_bundle",
        "quiesce_timeout_ms",
        "max_items_per_context",
        "max_tokens_per_context",
        "suggestions",
        "automations_enabled",
        "public_repo_event_triggers",
        "local_collection",
        "training_export",
        "retain_days",
        "semantic_targets_required_when_available",
        "coordinate_fallback",
        "recording",
        "recording_redaction",
        "desktop",
        "vision",
        "default_isolation",
        "clipboard",
        "file_chooser_roots",
        "max_full_screenshots_per_minute",
        "prefer_visual_delta",
        "models",
        "context",
        "agents",
        "sandbox",
        "telemetry",
        "daemon",
        "handoff",
        "knowledge",
        "playbooks",
        "trajectory",
        "computer_use",
        "schema",
    ];
    PROBES
        .iter()
        .any(|child| RapidConfig::is_known_key_path(&format!("{path}.{child}")))
}

fn parse_toml_table(
    source: ConfigOrigin,
    body: &str,
) -> Result<Map<String, Value>, ConfigLoadError> {
    if body.bytes().all(|b| b.is_ascii_whitespace()) {
        return Ok(Map::new());
    }
    let parsed: toml::Value =
        toml::from_str(body).map_err(|_| ConfigLoadError::InvalidSyntax { source })?;
    let json =
        serde_json::to_value(parsed).map_err(|_| ConfigLoadError::InvalidSyntax { source })?;
    match json {
        Value::Object(map) => Ok(map),
        _ => Err(ConfigLoadError::InvalidSyntax { source }),
    }
}

fn parse_override_literal(raw: &str) -> Value {
    let wrapped = format!("__v = {raw}");
    if let Ok(toml::Value::Table(mut table)) = toml::from_str::<toml::Value>(&wrapped)
        && let Some(parsed) = table.remove("__v")
        && let Ok(json) = serde_json::to_value(parsed)
    {
        return json;
    }
    Value::String(raw.to_owned())
}

fn overlay_map(
    dest: &mut Map<String, Value>,
    src: &Map<String, Value>,
    prefix: &str,
    source: ConfigOrigin,
    provenance: &mut BTreeMap<String, ConfigOrigin>,
    unknown_keys: &mut Vec<UnknownConfigKey>,
    cancel: &CancellationToken,
) -> Result<(), ConfigLoadError> {
    cancel.check()?;
    for (key, value) in src {
        let path = dotted(prefix, key);
        if !RapidConfig::is_known_key_path(&path) {
            if unknown_keys.len() < MAX_UNKNOWN_KEYS {
                unknown_keys.push(UnknownConfigKey { source, key: path });
            }
            continue;
        }
        match value {
            Value::Object(child) if dest.get(key).is_none_or(Value::is_object) => {
                let dest_child = dest
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                let Some(dest_map) = dest_child.as_object_mut() else {
                    return Err(ConfigLoadError::TypeMismatch { source, key: path });
                };
                overlay_map(
                    dest_map,
                    child,
                    &path,
                    source,
                    provenance,
                    unknown_keys,
                    cancel,
                )?;
            }
            _ => {
                if dest.get(key).is_some_and(Value::is_object) {
                    return Err(ConfigLoadError::TypeMismatch { source, key: path });
                }
                dest.insert(key.clone(), value.clone());
                provenance.insert(path, source);
            }
        }
    }
    Ok(())
}

fn set_leaf(
    root: &mut Map<String, Value>,
    path: &str,
    value: Value,
    source: ConfigOrigin,
) -> Result<(), ConfigLoadError> {
    let mut parts = path.split('.');
    let Some(first) = parts.next() else {
        return Err(ConfigLoadError::UnknownOverride {
            source,
            key: path.to_owned(),
        });
    };
    let mut current = root;
    let mut part = first;
    loop {
        match parts.next() {
            None => {
                if current.get(part).is_some_and(Value::is_object) {
                    return Err(ConfigLoadError::TypeMismatch {
                        source,
                        key: path.to_owned(),
                    });
                }
                current.insert(part.to_owned(), value);
                return Ok(());
            }
            Some(next) => {
                let entry = current
                    .entry(part.to_owned())
                    .or_insert_with(|| Value::Object(Map::new()));
                let Some(child) = entry.as_object_mut() else {
                    return Err(ConfigLoadError::TypeMismatch {
                        source,
                        key: path.to_owned(),
                    });
                };
                current = child;
                part = next;
            }
        }
    }
}

fn validate_layer(root: &Map<String, Value>, source: ConfigOrigin) -> Result<(), ConfigLoadError> {
    match RapidConfig::from_json_value(Value::Object(root.clone())) {
        Ok(_) => Ok(()),
        Err(err) => Err(map_decode(source, err)),
    }
}

fn map_decode(source: ConfigOrigin, err: ConfigError) -> ConfigLoadError {
    match err {
        ConfigError::InvalidSyntax => ConfigLoadError::InvalidSyntax { source },
        ConfigError::TypeMismatch { key } => ConfigLoadError::TypeMismatch { source, key },
        ConfigError::InvalidValue { key } => ConfigLoadError::InvalidValue { source, key },
        ConfigError::UnsupportedSchema { found } => {
            ConfigLoadError::UnsupportedSchema { source, found }
        }
        ConfigError::UnknownKeys { keys } => ConfigLoadError::UnknownOverride {
            source,
            key: keys.into_iter().next().unwrap_or_default(),
        },
    }
}

fn map_internal(err: ConfigError) -> ConfigLoadError {
    map_decode(ConfigOrigin::Defaults, err)
}

fn record_leaves(
    value: &Map<String, Value>,
    prefix: &str,
    origin: ConfigOrigin,
    out: &mut BTreeMap<String, ConfigOrigin>,
) {
    for (key, child) in value {
        let path = dotted(prefix, key);
        match child {
            Value::Object(map) => record_leaves(map, &path, origin, out),
            _ => {
                out.insert(path, origin);
            }
        }
    }
}

fn dotted(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn resolve_known_path(raw: &str) -> Option<String> {
    if RapidConfig::is_known_key_path(raw) && is_assignable_leaf(raw) {
        return Some(raw.to_owned());
    }
    let positions: Vec<usize> = raw.match_indices('_').map(|(i, _)| i).collect();
    if positions.len() > 16 {
        return None;
    }
    let mut found = None;
    let combinations = 1u32.checked_shl(positions.len() as u32)?;
    for mask in 0..combinations {
        let mut candidate = raw.to_owned();
        for (bit, &idx) in positions.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                candidate.replace_range(idx..idx + 1, ".");
            }
        }
        if RapidConfig::is_known_key_path(&candidate) && is_assignable_leaf(&candidate) {
            if found.is_some() {
                return None;
            }
            found = Some(candidate);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{EmbeddingsMode, NetworkMode};

    const SECRET: &str = "super-secret-password";

    struct PrecedenceCase {
        name: &'static str,
        workspace: Option<u32>,
        user: Option<u32>,
        env: Option<u32>,
        cli: Option<u32>,
        expected: u32,
        origin: ConfigOrigin,
    }

    fn toml_max_parallel(value: u32) -> String {
        format!("[agents]\nmax_parallel = {value}\n")
    }

    fn sources_for(case: &PrecedenceCase) -> ConfigSources {
        ConfigSources {
            workspace: case
                .workspace
                .map(|v| ConfigText::new(".rapidlm/config.toml", toml_max_parallel(v))),
            user: case
                .user
                .map(|v| ConfigText::new("user/config.toml", toml_max_parallel(v))),
            env: case
                .env
                .map(|v| ConfigOverride::new("RAPIDLM_AGENTS_MAX_PARALLEL", v.to_string()))
                .into_iter()
                .collect(),
            cli: case
                .cli
                .map(|v| ConfigOverride::new("agents.max_parallel", v.to_string()))
                .into_iter()
                .collect(),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn precedence_is_cli_gt_env_gt_user_gt_workspace_gt_defaults() {
        let cases = [
            PrecedenceCase {
                name: "defaults only",
                workspace: None,
                user: None,
                env: None,
                cli: None,
                expected: 4,
                origin: ConfigOrigin::Defaults,
            },
            PrecedenceCase {
                name: "workspace over defaults",
                workspace: Some(10),
                user: None,
                env: None,
                cli: None,
                expected: 10,
                origin: ConfigOrigin::Workspace,
            },
            PrecedenceCase {
                name: "user over workspace",
                workspace: Some(10),
                user: Some(20),
                env: None,
                cli: None,
                expected: 20,
                origin: ConfigOrigin::User,
            },
            PrecedenceCase {
                name: "env over user",
                workspace: Some(10),
                user: Some(20),
                env: Some(30),
                cli: None,
                expected: 30,
                origin: ConfigOrigin::Environment,
            },
            PrecedenceCase {
                name: "cli over env",
                workspace: Some(10),
                user: Some(20),
                env: Some(30),
                cli: Some(40),
                expected: 40,
                origin: ConfigOrigin::Cli,
            },
            PrecedenceCase {
                name: "cli over workspace without user or env",
                workspace: Some(10),
                user: None,
                env: None,
                cli: Some(40),
                expected: 40,
                origin: ConfigOrigin::Cli,
            },
            PrecedenceCase {
                name: "env over workspace",
                workspace: Some(10),
                user: None,
                env: Some(30),
                cli: None,
                expected: 30,
                origin: ConfigOrigin::Environment,
            },
            PrecedenceCase {
                name: "user over defaults",
                workspace: None,
                user: Some(20),
                env: None,
                cli: None,
                expected: 20,
                origin: ConfigOrigin::User,
            },
        ];

        for case in cases {
            let loaded = load_config(&sources_for(&case)).unwrap_or_else(|err| {
                panic!("{}: load failed: {err}", case.name);
            });
            assert_eq!(
                loaded.config.agents.max_parallel, case.expected,
                "{}",
                case.name
            );
            assert_eq!(
                loaded.origin("agents.max_parallel"),
                Some(case.origin),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn independent_fields_keep_their_own_origin() {
        let sources = ConfigSources {
            workspace: Some(ConfigText::new(
                ".rapidlm/config.toml",
                "[sandbox]\nnetwork = \"allow\"\n[agents]\nmax_parallel = 8\n",
            )),
            user: Some(ConfigText::new(
                "user.toml",
                "[models]\ndefault_policy = \"fast\"\n[agents]\nmax_write_parallel = 1\n",
            )),
            env: vec![ConfigOverride::new(
                "RAPIDLM_CONTEXT_MAX_INDEX_BYTES",
                "1024",
            )],
            cli: vec![ConfigOverride::new("daemon.enabled", "true")],
            cancel: CancellationToken::new(),
        };
        let loaded = load_config(&sources).expect("load");
        assert_eq!(loaded.config.sandbox.network, NetworkMode::Allow);
        assert_eq!(
            loaded.origin("sandbox.network"),
            Some(ConfigOrigin::Workspace)
        );
        assert_eq!(loaded.config.agents.max_parallel, 8);
        assert_eq!(
            loaded.origin("agents.max_parallel"),
            Some(ConfigOrigin::Workspace)
        );
        assert_eq!(loaded.config.models.default_policy.as_str(), "fast");
        assert_eq!(
            loaded.origin("models.default_policy"),
            Some(ConfigOrigin::User)
        );
        assert_eq!(loaded.config.agents.max_write_parallel, 1);
        assert_eq!(
            loaded.origin("agents.max_write_parallel"),
            Some(ConfigOrigin::User)
        );
        assert_eq!(loaded.config.context.max_index_bytes, 1024);
        assert_eq!(
            loaded.origin("context.max_index_bytes"),
            Some(ConfigOrigin::Environment)
        );
        assert!(loaded.config.daemon.enabled);
        assert_eq!(loaded.origin("daemon.enabled"), Some(ConfigOrigin::Cli));
        assert_eq!(loaded.config.context.embeddings, EmbeddingsMode::Auto);
        assert_eq!(
            loaded.origin("context.embeddings"),
            Some(ConfigOrigin::Defaults)
        );
    }

    #[test]
    fn parse_errors_name_source_and_key_without_secret_values() {
        let sources = ConfigSources {
            user: Some(ConfigText::new(
                "user.toml",
                format!("[agents]\nmax_parallel = \"{SECRET}\"\n"),
            )),
            ..ConfigSources::empty()
        };
        let err = load_config(&sources).expect_err("type");
        assert_eq!(
            err,
            ConfigLoadError::TypeMismatch {
                source: ConfigOrigin::User,
                key: "agents.max_parallel".to_owned(),
            }
        );
        let rendered = format!("{err}{err:?}{sources:?}");
        assert!(rendered.contains("user"));
        assert!(rendered.contains("agents.max_parallel"));
        assert!(!rendered.contains(SECRET));
    }

    #[test]
    fn env_parse_errors_omit_secret_values() {
        let sources = ConfigSources {
            env: vec![ConfigOverride::new("RAPIDLM_SANDBOX_NETWORK", SECRET)],
            ..ConfigSources::empty()
        };
        let err = load_config(&sources).expect_err("enum");
        assert_eq!(
            err,
            ConfigLoadError::InvalidValue {
                source: ConfigOrigin::Environment,
                key: "sandbox.network".to_owned(),
            }
        );
        let rendered = format!("{err}{err:?}{sources:?}");
        assert!(rendered.contains("environment"));
        assert!(rendered.contains("sandbox.network"));
        assert!(!rendered.contains(SECRET));
    }

    #[test]
    fn invalid_toml_names_source_without_body() {
        let sources = ConfigSources {
            workspace: Some(ConfigText::new(
                ".rapidlm/config.toml",
                format!("[[[ {SECRET}"),
            )),
            ..ConfigSources::empty()
        };
        let err = load_config(&sources).expect_err("syntax");
        assert_eq!(
            err,
            ConfigLoadError::InvalidSyntax {
                source: ConfigOrigin::Workspace,
            }
        );
        let rendered = format!("{err}{err:?}{sources:?}");
        assert!(rendered.contains("workspace"));
        assert!(!rendered.contains(SECRET));
    }

    #[test]
    fn unknown_workspace_keys_are_collected_not_merged() {
        let sources = ConfigSources {
            workspace: Some(ConfigText::new(
                ".rapidlm/config.toml",
                format!("api_key = \"{SECRET}\"\n[agents]\nmax_parallel = 9\n"),
            )),
            ..ConfigSources::empty()
        };
        let loaded = load_config(&sources).expect("load");
        assert_eq!(loaded.config.agents.max_parallel, 9);
        assert_eq!(
            loaded.unknown_keys,
            vec![UnknownConfigKey {
                source: ConfigOrigin::Workspace,
                key: "api_key".to_owned(),
            }]
        );
        let rendered = format!("{loaded:?}");
        assert!(!rendered.contains(SECRET));
    }

    #[test]
    fn cancelled_load_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = load_config(&ConfigSources {
            cancel,
            ..ConfigSources::empty()
        })
        .expect_err("cancelled");
        assert_eq!(err, ConfigLoadError::Cancelled);
    }

    #[test]
    fn oversized_document_is_rejected() {
        let body = "x = 1\n".repeat(MAX_CONFIG_DOCUMENT_BYTES);
        let err = load_config(&ConfigSources {
            user: Some(ConfigText::new("user.toml", body)),
            ..ConfigSources::empty()
        })
        .expect_err("too large");
        assert_eq!(
            err,
            ConfigLoadError::SourceTooLarge {
                source: ConfigOrigin::User,
            }
        );
    }

    #[test]
    fn env_name_maps_nested_and_underscored_leaves() {
        assert_eq!(
            config_key_from_env_name("RAPIDLM_AGENTS_MAX_PARALLEL").as_deref(),
            Some("agents.max_parallel")
        );
        assert_eq!(
            config_key_from_env_name("RAPIDLM_CONTEXT_MAX_INDEX_BYTES").as_deref(),
            Some("context.max_index_bytes")
        );
        assert_eq!(
            config_key_from_env_name("RAPIDLM_AGENTS_POOL_MAX_BACKGROUND").as_deref(),
            Some("agents.pool.max_background")
        );
        assert_eq!(
            config_key_from_env_name("RAPIDLM_COMPUTER_USE_DESKTOP_CLIPBOARD").as_deref(),
            Some("computer_use.desktop.clipboard")
        );
        assert_eq!(config_key_from_env_name("RAPIDLM_NOT_A_KEY"), None);
        assert_eq!(config_key_from_env_name("OTHER_AGENTS_MAX_PARALLEL"), None);
    }

    #[test]
    fn unknown_cli_key_is_an_error() {
        let err = load_config(&ConfigSources {
            cli: vec![ConfigOverride::new("not.a.key", "1")],
            ..ConfigSources::empty()
        })
        .expect_err("unknown");
        assert_eq!(
            err,
            ConfigLoadError::UnknownOverride {
                source: ConfigOrigin::Cli,
                key: "not.a.key".to_owned(),
            }
        );
    }
}
