//! Versioned user/workspace configuration types and documented defaults.
//!
//! Wire objects use snake_case JSON. Workspace files are TOML at load time; this
//! module decodes an equivalent object tree so unknown keys are detectable.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

/// Configuration schema version carried on [`RapidConfig::schema`].
pub const CONFIG_SCHEMA: u16 = 1;

/// Documented default for `context.max_index_bytes`.
pub const DEFAULT_MAX_INDEX_BYTES: u64 = 2_147_483_648;

/// Default named model policy (`models.default_policy`).
pub const DEFAULT_MODEL_POLICY: &str = "balanced";

/// Default desktop file-chooser roots.
pub const DEFAULT_FILE_CHOOSER_ROOTS: &[&str] = &["${workspace}", "${artifact_staging}"];

const MAX_UNKNOWN_KEYS: usize = 64;

macro_rules! string_enum {
    (
        $(#[$enum_meta:meta])*
        $name:ident,
        $err:ident,
        $expect:literal,
        default = $default:ident,
        { $($variant:ident => $wire:literal),+ $(,)? }
    ) => {
        $(#[$enum_meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $($variant),+
        }

        /// Parse failure for an unknown `$name` wire string.
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $err;

        impl $name {
            /// Every currently defined variant.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// Canonical snake/kebab-case wire form.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::$default
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = $err;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                for item in Self::ALL {
                    if item.as_str() == s {
                        return Ok(*item);
                    }
                }
                Err($err)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse()
                    .map_err(|_| de::Error::unknown_variant(&raw, &[$expect]))
            }
        }

        impl fmt::Display for $err {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!("unknown ", stringify!($name), " value"))
            }
        }

        impl Error for $err {}
    };
}

string_enum!(
    /// Optional embedding index mode. Offline retrieval remains valid when off.
    EmbeddingsMode,
    EmbeddingsModeParseError,
    "an embeddings mode",
    default = Auto,
    { Auto => "auto", Off => "off" }
);

string_enum!(
    /// Experimental verified-orchestration activation. Off is the default path.
    OrchestrationMode,
    OrchestrationModeParseError,
    "an orchestration mode",
    default = Off,
    { Off => "off", Verified => "verified" }
);

string_enum!(
    /// Sandbox backend selectable as `sandbox.default_tier`.
    SandboxTier,
    SandboxTierParseError,
    "a sandbox tier",
    default = Container,
    {
        HostRestricted => "host-restricted",
        Container => "container",
        Gvisor => "gvisor",
        RemoteWorker => "remote-worker",
    }
);

string_enum!(
    /// Network access default. Lower-trust layers may only tighten this.
    NetworkMode,
    NetworkModeParseError,
    "a network mode",
    default = Deny,
    { Deny => "deny", Allow => "allow" }
);

string_enum!(
    /// Telemetry transport. Export remains opt-in outside this local default.
    TelemetryMode,
    TelemetryModeParseError,
    "a telemetry mode",
    default = Local,
    { Local => "local" }
);

string_enum!(
    /// Whether telemetry may include content payloads.
    TelemetryContent,
    TelemetryContentParseError,
    "a telemetry content mode",
    default = Off,
    { Off => "off" }
);

string_enum!(
    /// Default role for persistent background pool agents.
    BackgroundAgentRole,
    BackgroundAgentRoleParseError,
    "a background agent role",
    default = ReadOnly,
    { ReadOnly => "read_only" }
);

string_enum!(
    /// How Knowledge suggestions enter a session.
    KnowledgeSuggestions,
    KnowledgeSuggestionsParseError,
    "a knowledge suggestion mode",
    default = ReviewRequired,
    { ReviewRequired => "review_required" }
);

string_enum!(
    /// Public repository / comment automation trigger policy.
    PublicRepoEventTriggers,
    PublicRepoEventTriggersParseError,
    "a public-repo event trigger policy",
    default = Deny,
    { Deny => "deny", Allow => "allow" }
);

string_enum!(
    /// Local trajectory collection scope.
    TrajectoryCollection,
    TrajectoryCollectionParseError,
    "a trajectory collection mode",
    default = EvalsAndOptInSessions,
    { EvalsAndOptInSessions => "evals_and_opt_in_sessions" }
);

string_enum!(
    /// When coordinate fallback is permitted for Computer Use.
    CoordinateFallback,
    CoordinateFallbackParseError,
    "a coordinate fallback policy",
    default = AskOnSensitive,
    { AskOnSensitive => "ask_on_sensitive" }
);

string_enum!(
    /// Computer Use recording policy.
    ComputerUseRecording,
    ComputerUseRecordingParseError,
    "a computer-use recording policy",
    default = OnForE2e,
    { OnForE2e => "on_for_e2e" }
);

string_enum!(
    /// Whether Computer Use recordings must be redacted.
    RecordingRedaction,
    RecordingRedactionParseError,
    "a recording redaction policy",
    default = Required,
    { Required => "required" }
);

string_enum!(
    /// Desktop Computer Use isolation default.
    DesktopIsolation,
    DesktopIsolationParseError,
    "a desktop isolation tier",
    default = GuiSandbox,
    { GuiSandbox => "gui_sandbox" }
);

string_enum!(
    /// Desktop clipboard access.
    ClipboardMode,
    ClipboardModeParseError,
    "a clipboard mode",
    default = Deny,
    { Deny => "deny", Allow => "allow" }
);

/// Named model-routing policy. Open string; default is `balanced`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelPolicyName(String);

/// Parse failure for an empty model policy name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelPolicyNameParseError;

impl ModelPolicyName {
    /// Canonical default policy name.
    pub const BALANCED: &'static str = DEFAULT_MODEL_POLICY;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ModelPolicyName {
    fn default() -> Self {
        Self(DEFAULT_MODEL_POLICY.to_owned())
    }
}

impl fmt::Display for ModelPolicyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ModelPolicyName {
    type Err = ModelPolicyNameParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            Err(ModelPolicyNameParseError)
        } else {
            Ok(Self(s.to_owned()))
        }
    }
}

impl fmt::Display for ModelPolicyNameParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("model policy name must be non-empty")
    }
}

impl Error for ModelPolicyNameParseError {}

/// Effective RapidLM configuration after defaults are applied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RapidConfig {
    pub schema: u16,
    pub models: ModelsConfig,
    pub context: ContextConfig,
    pub agents: AgentsConfig,
    pub sandbox: SandboxConfig,
    pub telemetry: TelemetryConfig,
    pub daemon: DaemonConfig,
    pub handoff: HandoffConfig,
    pub knowledge: KnowledgeConfig,
    pub playbooks: PlaybooksConfig,
    pub trajectory: TrajectoryConfig,
    pub computer_use: ComputerUseConfig,
    pub orchestration: OrchestrationConfig,
}

/// `[models]` section.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    pub default_policy: ModelPolicyName,
}

/// `[context]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub embeddings: EmbeddingsMode,
    pub max_index_bytes: u64,
}

/// `[agents]` section, including V2 pool/managed tables.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    pub max_parallel: u32,
    pub max_write_parallel: u32,
    pub pool: AgentPoolConfig,
    pub managed: ManagedAgentsConfig,
}

/// `[agents.pool]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPoolConfig {
    pub enabled: bool,
    pub max_background: u32,
    pub background_default_role: BackgroundAgentRole,
    pub mailbox_max_messages: u32,
    pub mailbox_max_inline_bytes: u32,
}

/// `[agents.managed]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedAgentsConfig {
    pub max_parallel: u32,
    pub max_write_parallel: u32,
    pub clean_context: bool,
    pub max_child_context_tokens: u32,
}

/// `[sandbox]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub default_tier: SandboxTier,
    pub network: NetworkMode,
}

/// `[telemetry]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    pub mode: TelemetryMode,
    pub content: TelemetryContent,
}

/// `[daemon]` section.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub enabled: bool,
}

/// `[handoff]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HandoffConfig {
    pub enabled: bool,
    pub require_signed_bundle: bool,
    pub quiesce_timeout_ms: u32,
}

/// `[knowledge]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeConfig {
    pub enabled: bool,
    pub max_items_per_context: u32,
    pub max_tokens_per_context: u32,
    pub suggestions: KnowledgeSuggestions,
}

/// `[playbooks]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybooksConfig {
    pub automations_enabled: bool,
    pub public_repo_event_triggers: PublicRepoEventTriggers,
}

/// `[trajectory]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrajectoryConfig {
    pub local_collection: TrajectoryCollection,
    pub training_export: bool,
    pub retain_days: u32,
}

/// `[computer_use]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputerUseConfig {
    pub semantic_targets_required_when_available: bool,
    pub coordinate_fallback: CoordinateFallback,
    pub recording: ComputerUseRecording,
    pub recording_redaction: RecordingRedaction,
    pub desktop: ComputerUseDesktopConfig,
    pub vision: ComputerUseVisionConfig,
}

/// `[computer_use.desktop]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputerUseDesktopConfig {
    pub default_isolation: DesktopIsolation,
    pub clipboard: ClipboardMode,
    pub file_chooser_roots: Vec<String>,
}

/// `[computer_use.vision]` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputerUseVisionConfig {
    pub max_full_screenshots_per_minute: u32,
    pub prefer_visual_delta: bool,
}

/// Decode result: typed config plus unknown key paths for warning vs `--strict`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigDocument {
    pub config: RapidConfig,
    pub unknown_keys: Vec<String>,
}

/// Typed configuration decode/validation failure. Messages never include values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    InvalidSyntax,
    TypeMismatch { key: String },
    InvalidValue { key: String },
    UnsupportedSchema { found: u16 },
    UnknownKeys { keys: Vec<String> },
}

impl Default for RapidConfig {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA,
            models: ModelsConfig::default(),
            context: ContextConfig::default(),
            agents: AgentsConfig::default(),
            sandbox: SandboxConfig::default(),
            telemetry: TelemetryConfig::default(),
            daemon: DaemonConfig::default(),
            handoff: HandoffConfig::default(),
            knowledge: KnowledgeConfig::default(),
            playbooks: PlaybooksConfig::default(),
            trajectory: TrajectoryConfig::default(),
            computer_use: ComputerUseConfig::default(),
            orchestration: OrchestrationConfig::default(),
        }
    }
}

/// `[orchestration]` section. Verified mode is experimental and off by default.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    pub mode: OrchestrationMode,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            embeddings: EmbeddingsMode::Auto,
            max_index_bytes: DEFAULT_MAX_INDEX_BYTES,
        }
    }
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            max_write_parallel: 2,
            pool: AgentPoolConfig::default(),
            managed: ManagedAgentsConfig::default(),
        }
    }
}

impl Default for AgentPoolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_background: 3,
            background_default_role: BackgroundAgentRole::ReadOnly,
            mailbox_max_messages: 512,
            mailbox_max_inline_bytes: 16_384,
        }
    }
}

impl Default for ManagedAgentsConfig {
    fn default() -> Self {
        Self {
            max_parallel: 6,
            max_write_parallel: 2,
            clean_context: true,
            max_child_context_tokens: 48_000,
        }
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            default_tier: SandboxTier::Container,
            network: NetworkMode::Deny,
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            mode: TelemetryMode::Local,
            content: TelemetryContent::Off,
        }
    }
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            require_signed_bundle: true,
            quiesce_timeout_ms: 30_000,
        }
    }
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_items_per_context: 8,
            max_tokens_per_context: 4_000,
            suggestions: KnowledgeSuggestions::ReviewRequired,
        }
    }
}

impl Default for PlaybooksConfig {
    fn default() -> Self {
        Self {
            automations_enabled: false,
            public_repo_event_triggers: PublicRepoEventTriggers::Deny,
        }
    }
}

impl Default for TrajectoryConfig {
    fn default() -> Self {
        Self {
            local_collection: TrajectoryCollection::EvalsAndOptInSessions,
            training_export: false,
            retain_days: 30,
        }
    }
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            semantic_targets_required_when_available: true,
            coordinate_fallback: CoordinateFallback::AskOnSensitive,
            recording: ComputerUseRecording::OnForE2e,
            recording_redaction: RecordingRedaction::Required,
            desktop: ComputerUseDesktopConfig::default(),
            vision: ComputerUseVisionConfig::default(),
        }
    }
}

impl Default for ComputerUseDesktopConfig {
    fn default() -> Self {
        Self {
            default_isolation: DesktopIsolation::GuiSandbox,
            clipboard: ClipboardMode::Deny,
            file_chooser_roots: DEFAULT_FILE_CHOOSER_ROOTS
                .iter()
                .map(|root| (*root).to_owned())
                .collect(),
        }
    }
}

impl Default for ComputerUseVisionConfig {
    fn default() -> Self {
        Self {
            max_full_screenshots_per_minute: 12,
            prefer_visual_delta: true,
        }
    }
}

impl ConfigDocument {
    pub fn has_unknown_keys(&self) -> bool {
        !self.unknown_keys.is_empty()
    }

    /// Error if any unknown keys were present (`rapid config validate --strict`).
    pub fn into_strict(self) -> Result<RapidConfig, ConfigError> {
        if self.unknown_keys.is_empty() {
            Ok(self.config)
        } else {
            Err(ConfigError::UnknownKeys {
                keys: self.unknown_keys,
            })
        }
    }
}

impl RapidConfig {
    /// Decode a JSON object. Missing keys take documented defaults.
    ///
    /// Unknown keys are collected, not rejected. Call
    /// [`ConfigDocument::into_strict`] for `--strict`.
    pub fn from_json_str(src: &str) -> Result<ConfigDocument, ConfigError> {
        let value: Value = serde_json::from_str(src).map_err(|_| ConfigError::InvalidSyntax)?;
        Self::from_json_value(value)
    }

    /// Decode and reject unknown keys.
    pub fn from_json_str_strict(src: &str) -> Result<Self, ConfigError> {
        Self::from_json_str(src)?.into_strict()
    }

    pub fn from_json_value(value: Value) -> Result<ConfigDocument, ConfigError> {
        let Some(obj) = value.as_object() else {
            return Err(ConfigError::InvalidSyntax);
        };

        let mut unknown_keys = Vec::new();
        let mut error = None;
        walk_object(obj, &ROOT_SPEC, "", &mut unknown_keys, &mut error);
        if let Some(error) = error {
            return Err(error);
        }

        let config: RapidConfig =
            serde_json::from_value(value).map_err(|_| ConfigError::InvalidSyntax)?;
        Ok(ConfigDocument {
            config,
            unknown_keys,
        })
    }

    /// Whether `path` is a known dotted key in schema v1 (including V2 tables).
    pub fn is_known_key_path(path: &str) -> bool {
        is_known_path(&ROOT_SPEC, path)
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSyntax => f.write_str("invalid configuration syntax"),
            Self::TypeMismatch { key } => write!(f, "invalid type for {key}"),
            Self::InvalidValue { key } => write!(f, "invalid value for {key}"),
            Self::UnsupportedSchema { found } => {
                write!(f, "unsupported configuration schema {found}")
            }
            Self::UnknownKeys { keys } => {
                f.write_str("unknown configuration keys: ")?;
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(key)?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ConfigError {}

#[derive(Clone, Copy)]
enum LeafKind {
    SchemaVersion,
    U32,
    U64,
    Bool,
    PolicyName,
    StringList,
    Enum(&'static [&'static str]),
}

struct TableSpec {
    leaves: &'static [(&'static str, LeafKind)],
    tables: &'static [(&'static str, TableSpec)],
}

const ROOT_SPEC: TableSpec = TableSpec {
    leaves: &[("schema", LeafKind::SchemaVersion)],
    tables: &[
        (
            "models",
            TableSpec {
                leaves: &[("default_policy", LeafKind::PolicyName)],
                tables: &[],
            },
        ),
        (
            "context",
            TableSpec {
                leaves: &[
                    (
                        "embeddings",
                        LeafKind::Enum(&[
                            EmbeddingsMode::Auto.as_str(),
                            EmbeddingsMode::Off.as_str(),
                        ]),
                    ),
                    ("max_index_bytes", LeafKind::U64),
                ],
                tables: &[],
            },
        ),
        (
            "agents",
            TableSpec {
                leaves: &[
                    ("max_parallel", LeafKind::U32),
                    ("max_write_parallel", LeafKind::U32),
                ],
                tables: &[
                    (
                        "pool",
                        TableSpec {
                            leaves: &[
                                ("enabled", LeafKind::Bool),
                                ("max_background", LeafKind::U32),
                                (
                                    "background_default_role",
                                    LeafKind::Enum(&[BackgroundAgentRole::ReadOnly.as_str()]),
                                ),
                                ("mailbox_max_messages", LeafKind::U32),
                                ("mailbox_max_inline_bytes", LeafKind::U32),
                            ],
                            tables: &[],
                        },
                    ),
                    (
                        "managed",
                        TableSpec {
                            leaves: &[
                                ("max_parallel", LeafKind::U32),
                                ("max_write_parallel", LeafKind::U32),
                                ("clean_context", LeafKind::Bool),
                                ("max_child_context_tokens", LeafKind::U32),
                            ],
                            tables: &[],
                        },
                    ),
                ],
            },
        ),
        (
            "sandbox",
            TableSpec {
                leaves: &[
                    (
                        "default_tier",
                        LeafKind::Enum(&[
                            SandboxTier::HostRestricted.as_str(),
                            SandboxTier::Container.as_str(),
                            SandboxTier::Gvisor.as_str(),
                            SandboxTier::RemoteWorker.as_str(),
                        ]),
                    ),
                    (
                        "network",
                        LeafKind::Enum(&[NetworkMode::Deny.as_str(), NetworkMode::Allow.as_str()]),
                    ),
                ],
                tables: &[],
            },
        ),
        (
            "telemetry",
            TableSpec {
                leaves: &[
                    ("mode", LeafKind::Enum(&[TelemetryMode::Local.as_str()])),
                    ("content", LeafKind::Enum(&[TelemetryContent::Off.as_str()])),
                ],
                tables: &[],
            },
        ),
        (
            "daemon",
            TableSpec {
                leaves: &[("enabled", LeafKind::Bool)],
                tables: &[],
            },
        ),
        (
            "handoff",
            TableSpec {
                leaves: &[
                    ("enabled", LeafKind::Bool),
                    ("require_signed_bundle", LeafKind::Bool),
                    ("quiesce_timeout_ms", LeafKind::U32),
                ],
                tables: &[],
            },
        ),
        (
            "knowledge",
            TableSpec {
                leaves: &[
                    ("enabled", LeafKind::Bool),
                    ("max_items_per_context", LeafKind::U32),
                    ("max_tokens_per_context", LeafKind::U32),
                    (
                        "suggestions",
                        LeafKind::Enum(&[KnowledgeSuggestions::ReviewRequired.as_str()]),
                    ),
                ],
                tables: &[],
            },
        ),
        (
            "playbooks",
            TableSpec {
                leaves: &[
                    ("automations_enabled", LeafKind::Bool),
                    (
                        "public_repo_event_triggers",
                        LeafKind::Enum(&[
                            PublicRepoEventTriggers::Deny.as_str(),
                            PublicRepoEventTriggers::Allow.as_str(),
                        ]),
                    ),
                ],
                tables: &[],
            },
        ),
        (
            "trajectory",
            TableSpec {
                leaves: &[
                    (
                        "local_collection",
                        LeafKind::Enum(&[TrajectoryCollection::EvalsAndOptInSessions.as_str()]),
                    ),
                    ("training_export", LeafKind::Bool),
                    ("retain_days", LeafKind::U32),
                ],
                tables: &[],
            },
        ),
        (
            "computer_use",
            TableSpec {
                leaves: &[
                    ("semantic_targets_required_when_available", LeafKind::Bool),
                    (
                        "coordinate_fallback",
                        LeafKind::Enum(&[CoordinateFallback::AskOnSensitive.as_str()]),
                    ),
                    (
                        "recording",
                        LeafKind::Enum(&[ComputerUseRecording::OnForE2e.as_str()]),
                    ),
                    (
                        "recording_redaction",
                        LeafKind::Enum(&[RecordingRedaction::Required.as_str()]),
                    ),
                ],
                tables: &[
                    (
                        "desktop",
                        TableSpec {
                            leaves: &[
                                (
                                    "default_isolation",
                                    LeafKind::Enum(&[DesktopIsolation::GuiSandbox.as_str()]),
                                ),
                                (
                                    "clipboard",
                                    LeafKind::Enum(&[
                                        ClipboardMode::Deny.as_str(),
                                        ClipboardMode::Allow.as_str(),
                                    ]),
                                ),
                                ("file_chooser_roots", LeafKind::StringList),
                            ],
                            tables: &[],
                        },
                    ),
                    (
                        "vision",
                        TableSpec {
                            leaves: &[
                                ("max_full_screenshots_per_minute", LeafKind::U32),
                                ("prefer_visual_delta", LeafKind::Bool),
                            ],
                            tables: &[],
                        },
                    ),
                ],
            },
        ),
        (
            "orchestration",
            TableSpec {
                leaves: &[(
                    "mode",
                    LeafKind::Enum(&[
                        OrchestrationMode::Off.as_str(),
                        OrchestrationMode::Verified.as_str(),
                    ]),
                )],
                tables: &[],
            },
        ),
    ],
};

fn dotted(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn walk_object(
    obj: &serde_json::Map<String, Value>,
    spec: &TableSpec,
    prefix: &str,
    unknown: &mut Vec<String>,
    error: &mut Option<ConfigError>,
) {
    for (key, value) in obj {
        let path = dotted(prefix, key);
        if let Some((_, kind)) = spec.leaves.iter().find(|(name, _)| *name == key) {
            if error.is_none() {
                *error = check_leaf(*kind, value, &path);
            }
            continue;
        }
        if let Some((_, child)) = spec.tables.iter().find(|(name, _)| *name == key) {
            match value.as_object() {
                Some(child_obj) => walk_object(child_obj, child, &path, unknown, error),
                None => {
                    if error.is_none() {
                        *error = Some(ConfigError::TypeMismatch { key: path });
                    }
                }
            }
            continue;
        }
        if unknown.len() < MAX_UNKNOWN_KEYS {
            unknown.push(path);
        }
    }
}

fn check_leaf(kind: LeafKind, value: &Value, key: &str) -> Option<ConfigError> {
    match kind {
        LeafKind::SchemaVersion => match value.as_u64() {
            Some(found) if found <= u64::from(u16::MAX) => {
                let found = found as u16;
                if found == CONFIG_SCHEMA {
                    None
                } else {
                    Some(ConfigError::UnsupportedSchema { found })
                }
            }
            _ => Some(ConfigError::TypeMismatch {
                key: key.to_owned(),
            }),
        },
        LeafKind::U32 => match value.as_u64() {
            Some(v) if v <= u64::from(u32::MAX) => None,
            _ => Some(ConfigError::TypeMismatch {
                key: key.to_owned(),
            }),
        },
        LeafKind::U64 => match value.as_u64() {
            Some(_) => None,
            None => Some(ConfigError::TypeMismatch {
                key: key.to_owned(),
            }),
        },
        LeafKind::Bool => {
            if value.is_boolean() {
                None
            } else {
                Some(ConfigError::TypeMismatch {
                    key: key.to_owned(),
                })
            }
        }
        LeafKind::PolicyName => match value.as_str() {
            Some(name) if !name.is_empty() => None,
            Some(_) => Some(ConfigError::InvalidValue {
                key: key.to_owned(),
            }),
            None => Some(ConfigError::TypeMismatch {
                key: key.to_owned(),
            }),
        },
        LeafKind::StringList => match value.as_array() {
            Some(items) if items.iter().all(Value::is_string) => None,
            _ => Some(ConfigError::TypeMismatch {
                key: key.to_owned(),
            }),
        },
        LeafKind::Enum(allowed) => match value.as_str() {
            Some(found) if allowed.contains(&found) => None,
            Some(_) => Some(ConfigError::InvalidValue {
                key: key.to_owned(),
            }),
            None => Some(ConfigError::TypeMismatch {
                key: key.to_owned(),
            }),
        },
    }
}

fn is_known_path(spec: &TableSpec, path: &str) -> bool {
    let mut current = spec;
    let mut parts = path.split('.');
    let Some(mut part) = parts.next() else {
        return false;
    };
    loop {
        match parts.next() {
            None => {
                return current.leaves.iter().any(|(name, _)| *name == part)
                    || current.tables.iter().any(|(name, _)| *name == part);
            }
            Some(next) => {
                let Some((_, child)) = current.tables.iter().find(|(name, _)| *name == part) else {
                    return false;
                };
                current = child;
                part = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_DEFAULT: &str = concat!(
        r#"{"schema":1,"#,
        r#""models":{"default_policy":"balanced"},"#,
        r#""context":{"embeddings":"auto","max_index_bytes":2147483648},"#,
        r#""agents":{"max_parallel":4,"max_write_parallel":2,"#,
        r#""pool":{"enabled":true,"max_background":3,"background_default_role":"read_only","mailbox_max_messages":512,"mailbox_max_inline_bytes":16384},"#,
        r#""managed":{"max_parallel":6,"max_write_parallel":2,"clean_context":true,"max_child_context_tokens":48000}},"#,
        r#""sandbox":{"default_tier":"container","network":"deny"},"#,
        r#""telemetry":{"mode":"local","content":"off"},"#,
        r#""daemon":{"enabled":false},"#,
        r#""handoff":{"enabled":true,"require_signed_bundle":true,"quiesce_timeout_ms":30000},"#,
        r#""knowledge":{"enabled":true,"max_items_per_context":8,"max_tokens_per_context":4000,"suggestions":"review_required"},"#,
        r#""playbooks":{"automations_enabled":false,"public_repo_event_triggers":"deny"},"#,
        r#""trajectory":{"local_collection":"evals_and_opt_in_sessions","training_export":false,"retain_days":30},"#,
        r#""computer_use":{"semantic_targets_required_when_available":true,"coordinate_fallback":"ask_on_sensitive","recording":"on_for_e2e","recording_redaction":"required","#,
        r#""desktop":{"default_isolation":"gui_sandbox","clipboard":"deny","file_chooser_roots":["${workspace}","${artifact_staging}"]},"#,
        r#""vision":{"max_full_screenshots_per_minute":12,"prefer_visual_delta":true}},"#,
        r#""orchestration":{"mode":"off"}}"#
    );

    /// JSON equivalent of the v1 example in `data-models/config-schema.md`.
    const SCHEMA_V1_EXAMPLE: &str = r#"{
        "schema": 1,
        "models": { "default_policy": "balanced" },
        "context": { "embeddings": "auto", "max_index_bytes": 2147483648 },
        "agents": { "max_parallel": 4, "max_write_parallel": 2 },
        "sandbox": { "default_tier": "container", "network": "deny" },
        "telemetry": { "mode": "local", "content": "off" },
        "daemon": { "enabled": false }
    }"#;

    /// JSON equivalent of the V2 additive example in `data-models/config-schema.md`.
    const SCHEMA_V2_EXAMPLE: &str = r#"{
        "agents": {
            "pool": {
                "enabled": true,
                "max_background": 3,
                "background_default_role": "read_only",
                "mailbox_max_messages": 512,
                "mailbox_max_inline_bytes": 16384
            },
            "managed": {
                "max_parallel": 6,
                "max_write_parallel": 2,
                "clean_context": true,
                "max_child_context_tokens": 48000
            }
        },
        "handoff": {
            "enabled": true,
            "require_signed_bundle": true,
            "quiesce_timeout_ms": 30000
        },
        "knowledge": {
            "enabled": true,
            "max_items_per_context": 8,
            "max_tokens_per_context": 4000,
            "suggestions": "review_required"
        },
        "playbooks": {
            "automations_enabled": false,
            "public_repo_event_triggers": "deny"
        },
        "trajectory": {
            "local_collection": "evals_and_opt_in_sessions",
            "training_export": false,
            "retain_days": 30
        },
        "computer_use": {
            "semantic_targets_required_when_available": true,
            "coordinate_fallback": "ask_on_sensitive",
            "recording": "on_for_e2e",
            "recording_redaction": "required",
            "desktop": {
                "default_isolation": "gui_sandbox",
                "clipboard": "deny",
                "file_chooser_roots": ["${workspace}", "${artifact_staging}"]
            },
            "vision": {
                "max_full_screenshots_per_minute": 12,
                "prefer_visual_delta": true
            }
        }
    }"#;

    const SECRET_CANARY: &str = "super-secret-password";

    #[test]
    fn defaults_match_config_schema() {
        let cfg = RapidConfig::default();
        assert_eq!(cfg.schema, CONFIG_SCHEMA);
        assert_eq!(cfg.models.default_policy.as_str(), "balanced");
        assert_eq!(cfg.context.embeddings, EmbeddingsMode::Auto);
        assert_eq!(cfg.context.max_index_bytes, 2_147_483_648);
        assert_eq!(cfg.agents.max_parallel, 4);
        assert_eq!(cfg.agents.max_write_parallel, 2);
        assert_eq!(cfg.sandbox.default_tier, SandboxTier::Container);
        assert_eq!(cfg.sandbox.network, NetworkMode::Deny);
        assert_eq!(cfg.telemetry.mode, TelemetryMode::Local);
        assert_eq!(cfg.telemetry.content, TelemetryContent::Off);
        assert!(!cfg.daemon.enabled);

        assert!(cfg.agents.pool.enabled);
        assert_eq!(cfg.agents.pool.max_background, 3);
        assert_eq!(
            cfg.agents.pool.background_default_role,
            BackgroundAgentRole::ReadOnly
        );
        assert_eq!(cfg.agents.pool.mailbox_max_messages, 512);
        assert_eq!(cfg.agents.pool.mailbox_max_inline_bytes, 16_384);
        assert_eq!(cfg.agents.managed.max_parallel, 6);
        assert_eq!(cfg.agents.managed.max_write_parallel, 2);
        assert!(cfg.agents.managed.clean_context);
        assert_eq!(cfg.agents.managed.max_child_context_tokens, 48_000);

        assert!(cfg.handoff.enabled);
        assert!(cfg.handoff.require_signed_bundle);
        assert_eq!(cfg.handoff.quiesce_timeout_ms, 30_000);
        assert!(cfg.knowledge.enabled);
        assert_eq!(cfg.knowledge.max_items_per_context, 8);
        assert_eq!(cfg.knowledge.max_tokens_per_context, 4_000);
        assert_eq!(
            cfg.knowledge.suggestions,
            KnowledgeSuggestions::ReviewRequired
        );
        assert!(!cfg.playbooks.automations_enabled);
        assert_eq!(
            cfg.playbooks.public_repo_event_triggers,
            PublicRepoEventTriggers::Deny
        );
        assert_eq!(
            cfg.trajectory.local_collection,
            TrajectoryCollection::EvalsAndOptInSessions
        );
        assert!(!cfg.trajectory.training_export);
        assert_eq!(cfg.trajectory.retain_days, 30);
        assert!(cfg.computer_use.semantic_targets_required_when_available);
        assert_eq!(
            cfg.computer_use.coordinate_fallback,
            CoordinateFallback::AskOnSensitive
        );
        assert_eq!(cfg.computer_use.recording, ComputerUseRecording::OnForE2e);
        assert_eq!(
            cfg.computer_use.recording_redaction,
            RecordingRedaction::Required
        );
        assert_eq!(
            cfg.computer_use.desktop.default_isolation,
            DesktopIsolation::GuiSandbox
        );
        assert_eq!(cfg.computer_use.desktop.clipboard, ClipboardMode::Deny);
        assert_eq!(
            cfg.computer_use.desktop.file_chooser_roots,
            vec!["${workspace}".to_owned(), "${artifact_staging}".to_owned()]
        );
        assert_eq!(cfg.computer_use.vision.max_full_screenshots_per_minute, 12);
        assert!(cfg.computer_use.vision.prefer_visual_delta);
    }

    #[test]
    fn schema_examples_decode_to_defaults() {
        let v1 = RapidConfig::from_json_str(SCHEMA_V1_EXAMPLE)
            .expect("v1 example")
            .into_strict()
            .expect("v1 strict");
        let v2 = RapidConfig::from_json_str(SCHEMA_V2_EXAMPLE)
            .expect("v2 example")
            .into_strict()
            .expect("v2 strict");
        assert_eq!(v1, RapidConfig::default());
        assert_eq!(v2, RapidConfig::default());
    }

    #[test]
    fn empty_object_is_documented_defaults() {
        let doc = RapidConfig::from_json_str("{}").expect("empty");
        assert!(!doc.has_unknown_keys());
        assert_eq!(doc.config, RapidConfig::default());
    }

    #[test]
    fn golden_json_round_trips() {
        let json = serde_json::to_string(&RapidConfig::default()).expect("serialize");
        assert_eq!(json, GOLDEN_DEFAULT);
        let decoded = serde_json::from_str::<RapidConfig>(GOLDEN_DEFAULT).expect("deserialize");
        assert_eq!(decoded, RapidConfig::default());
        let via_api = RapidConfig::from_json_str_strict(GOLDEN_DEFAULT).expect("strict");
        assert_eq!(via_api, RapidConfig::default());
    }

    #[test]
    fn unknown_fields_are_detectable_for_strict_validation() {
        let src = r#"{"models":{"default_policy":"balanced","alias":"x"},"extra":1}"#;
        let doc = RapidConfig::from_json_str(src).expect("lenient");
        assert_eq!(
            doc.unknown_keys,
            vec!["extra".to_owned(), "models.alias".to_owned()]
        );
        assert_eq!(doc.config.models.default_policy.as_str(), "balanced");
        let err = RapidConfig::from_json_str_strict(src).expect_err("strict");
        assert_eq!(
            err,
            ConfigError::UnknownKeys {
                keys: vec!["extra".to_owned(), "models.alias".to_owned()]
            }
        );
        assert!(RapidConfig::is_known_key_path("models.default_policy"));
        assert!(!RapidConfig::is_known_key_path("models.alias"));
        assert!(RapidConfig::is_known_key_path(
            "computer_use.desktop.clipboard"
        ));
    }

    #[test]
    fn unknown_table_is_reported_at_the_parent_path() {
        let doc = RapidConfig::from_json_str(r#"{"plugins":{"enabled":true}}"#).expect("lenient");
        assert_eq!(doc.unknown_keys, vec!["plugins".to_owned()]);
    }

    #[test]
    fn invalid_enum_reports_key_without_value() {
        let src = r#"{"sandbox":{"default_tier":"host"}}"#;
        let err = RapidConfig::from_json_str(src).expect_err("tier");
        assert_eq!(
            err,
            ConfigError::InvalidValue {
                key: "sandbox.default_tier".to_owned()
            }
        );
        let rendered = format!("{err}{err:?}");
        assert!(!rendered.contains("host"));
        assert!(rendered.contains("sandbox.default_tier"));
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let err = RapidConfig::from_json_str(r#"{"schema":2}"#).expect_err("schema");
        assert_eq!(err, ConfigError::UnsupportedSchema { found: 2 });
    }

    #[test]
    fn type_mismatch_names_key_only() {
        let err = RapidConfig::from_json_str(r#"{"daemon":{"enabled":"yes"}}"#).expect_err("type");
        assert_eq!(
            err,
            ConfigError::TypeMismatch {
                key: "daemon.enabled".to_owned()
            }
        );
    }

    #[test]
    fn secret_values_are_not_copied_into_errors() {
        let src = format!(r#"{{"api_key":"{SECRET_CANARY}"}}"#);
        let doc = RapidConfig::from_json_str(&src).expect("lenient");
        assert_eq!(doc.unknown_keys, vec!["api_key".to_owned()]);
        let err = doc.into_strict().expect_err("strict");
        let rendered = format!("{err}{err:?}");
        assert!(rendered.contains("api_key"));
        assert!(!rendered.contains(SECRET_CANARY));
        assert!(!format!("{:?}", RapidConfig::default()).contains(SECRET_CANARY));
    }

    #[test]
    fn closed_enums_round_trip_and_reject_unknown() {
        for tier in SandboxTier::ALL {
            let json = serde_json::to_string(tier).expect("ser");
            assert_eq!(json, format!("\"{}\"", tier.as_str()));
            assert_eq!(
                serde_json::from_str::<SandboxTier>(&json).expect("de"),
                *tier
            );
        }
        assert!("container".parse::<SandboxTier>().expect("parse") == SandboxTier::Container);
        assert_eq!("host".parse::<SandboxTier>(), Err(SandboxTierParseError));
        assert_eq!(
            "".parse::<ModelPolicyName>(),
            Err(ModelPolicyNameParseError)
        );
        assert_eq!(
            "custom".parse::<ModelPolicyName>().expect("name").as_str(),
            "custom"
        );
    }
}
