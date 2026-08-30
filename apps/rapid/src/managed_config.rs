//! Enterprise managed-config layer: a managed policy document that gates
//! user model configuration and the permission mode (Modbit `CAP-001`:
//! hard invariants merge with lower-trust layers only ever restricting,
//! never widening — this is that merge, scoped so far to the fields below).
//!
//! Administrators point `RAPIDLM_MANAGED_CONFIG` at a policy file. Gates are
//! enforced, not advisory:
//!
//! * `locked_default` beats everything — managed > env > user > built-in;
//! * `allowed_providers` is an allowlist; a user model entry on a
//!   non-allowed provider is a hard, field-level error with a remediation
//!   string (fail closed — the turn never runs on a forbidden provider);
//! * `min_reasoning_effort` raises any configured effort below the floor;
//! * `max_permission_mode` lowers a resolved permission mode that exceeds
//!   the ceiling (`gate_permission_mode`) — a project's `.rapidlm/
//!   settings.json` or `RAPIDLM_PERMISSION_MODE` can request `bypassPermissions`,
//!   but never actually get more than an admin allows;
//! * `denied_tools` bans specific tools outright, applied as
//!   `PermissionLattice::with_denied_tools` — checked before every rule,
//!   grant, and mode (including `bypassPermissions`), so nothing downstream
//!   can ever re-enable a banned tool;
//! * `confine_writes_to` applies `PermissionLattice::with_admin_write_scope`
//!   — a deployment-wide write confinement independent of (and checked
//!   before) any per-`task_spawn` `write_scope`, so a subagent's own scope
//!   argument can only add a further restriction inside it, never widen
//!   past it.
//!
//! Every gate outcome is reported with field id, origin, and remediation so
//! operators can see exactly which layer decided what.

use std::fmt;

use llm_router::ReasoningEffort;

use crate::user_config::{
    ActiveModel, ConfigProvider, DEFAULT_MODEL_ENV, UserConfig, UserConfigError, env_value,
    resolve_active,
};

/// Environment variable naming the managed policy document.
pub const MANAGED_CONFIG_ENV: &str = "RAPIDLM_MANAGED_CONFIG";
/// Wire identity of the managed policy format.
pub const MANAGED_SCHEMA: &str = "rapidlm.managed_config.v1";

/// Which configuration layer decided a field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ConfigOrigin {
    /// Built-in fallback.
    Default,
    /// The managed policy document.
    Managed,
    /// The user config document.
    User,
    /// A process environment override.
    Env,
}

impl ConfigOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Managed => "managed",
            Self::User => "user",
            Self::Env => "env",
        }
    }
}

/// A field-level gate failure: which field, which layer set it, what to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFieldError {
    /// Dotted field id (e.g. `local.provider`).
    pub field_id: String,
    /// Layer whose value violated the gate.
    pub origin: ConfigOrigin,
    pub reason: String,
    pub remediation: &'static str,
}

impl fmt::Display for ConfigFieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "managed gate violation at '{}' (origin={}): {}; remediation: {}",
            self.field_id, self.origin.as_str(), self.reason, self.remediation
        )
    }
}

impl std::error::Error for ConfigFieldError {}

/// Typed failures of the managed layer.
#[derive(Debug)]
pub enum ManagedConfigError {
    Io(std::io::Error),
    Parse {
        reason: String,
    },
    SchemaMismatch {
        found: String,
    },
    UnknownField {
        field: String,
    },
    /// The policy document itself is invalid; the field is a policy field.
    PolicyField(ConfigFieldError),
}

impl fmt::Display for ManagedConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "managed config unreadable: {err}"),
            Self::Parse { reason } => write!(f, "managed config is invalid TOML: {reason}"),
            Self::SchemaMismatch { found } => write!(
                f,
                "managed config schema '{found}' is unsupported; expected '{MANAGED_SCHEMA}'"
            ),
            Self::UnknownField { field } => {
                write!(f, "managed config has unknown field '{field}'")
            }
            Self::PolicyField(err) => write!(f, "managed config is invalid: {err}"),
        }
    }
}

impl std::error::Error for ManagedConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::PolicyField(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ManagedConfigError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// The managed policy as parsed from the document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedPolicy {
    /// `[models] default` the user may not change.
    locked_default: Option<String>,
    /// Provider allowlist; `None` allows every provider.
    allowed_providers: Option<Vec<String>>,
    /// Any configured effort below this floor is raised to it.
    min_reasoning_effort: Option<ReasoningEffort>,
    /// Ceiling on the resolved permission mode (Modbit `CAP-001`): a
    /// project's `.rapidlm/settings.json` or `RAPIDLM_PERMISSION_MODE` may
    /// only narrow what this allows, never widen past it.
    max_permission_mode: Option<crate::permissions::PermissionMode>,
    /// Tools banned outright regardless of project/user settings or mode
    /// (Modbit `CAP-001`), applied via `PermissionLattice::with_denied_tools`.
    denied_tools: Option<Vec<crate::permissions::ToolPattern>>,
    /// Workspace-relative path prefix every write is confined to, deployment
    /// -wide (Modbit `CAP-001`), applied via
    /// `PermissionLattice::with_admin_write_scope`.
    confine_writes_to: Option<String>,
}

impl ManagedPolicy {
    /// Parse the closed-schema policy document:
    ///
    /// ```toml
    /// schema = "rapidlm.managed_config.v1"
    /// [policy]
    /// locked_default = "cloud"
    /// allowed_providers = ["openai-compatible", "anthropic"]
    /// min_reasoning_effort = "high"
    /// max_permission_mode = "acceptEdits"
    /// denied_tools = ["shell_exec"]
    /// confine_writes_to = "src"
    /// ```
    pub fn parse(toml_str: &str) -> Result<Self, ManagedConfigError> {
        let value: toml::Value =
            toml::from_str(toml_str).map_err(|err| ManagedConfigError::Parse {
                reason: err.to_string(),
            })?;
        let table = value.as_table().ok_or_else(|| ManagedConfigError::Parse {
            reason: "top level must be a table".to_string(),
        })?;
        for key in table.keys() {
            if key != "schema" && key != "policy" {
                return Err(ManagedConfigError::UnknownField {
                    field: key.clone(),
                });
            }
        }
        let schema = table
            .get("schema")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| ManagedConfigError::Parse {
                reason: "missing schema marker".to_string(),
            })?;
        if schema != MANAGED_SCHEMA {
            return Err(ManagedConfigError::SchemaMismatch {
                found: schema.to_string(),
            });
        }
        let policy = table.get("policy").ok_or_else(|| ManagedConfigError::Parse {
            reason: "missing [policy] table".to_string(),
        })?;
        let policy = policy.as_table().ok_or_else(|| ManagedConfigError::Parse {
            reason: "[policy] must be a table".to_string(),
        })?;
        for key in policy.keys() {
            if !matches!(
                key.as_str(),
                "locked_default"
                    | "allowed_providers"
                    | "min_reasoning_effort"
                    | "max_permission_mode"
                    | "denied_tools"
                    | "confine_writes_to"
            ) {
                return Err(ManagedConfigError::UnknownField {
                    field: format!("policy.{key}"),
                });
            }
        }
        let locked_default = match policy.get("locked_default") {
            None => None,
            Some(raw) => {
                let raw = raw.as_str().ok_or_else(|| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.locked_default",
                        "must be a string naming a [model.<id>] table",
                    ))
                })?;
                if raw.trim().is_empty() {
                    return Err(ManagedConfigError::PolicyField(field_error(
                        "policy.locked_default",
                        "must not be empty",
                    )));
                }
                Some(raw.to_string())
            }
        };
        let allowed_providers = match policy.get("allowed_providers") {
            None => None,
            Some(raw) => {
                let entries =
                    raw.as_array()
                        .ok_or_else(|| {
                            ManagedConfigError::PolicyField(field_error(
                                "policy.allowed_providers",
                                "must be an array of provider names",
                            ))
                        })?;
                let mut names = Vec::with_capacity(entries.len());
                for entry in entries {
                    let name = entry.as_str().ok_or_else(|| {
                        ManagedConfigError::PolicyField(field_error(
                            "policy.allowed_providers",
                            "entries must be strings",
                        ))
                    })?;
                    if ConfigProvider::parse(name).is_none() {
                        return Err(ManagedConfigError::PolicyField(field_error(
                            "policy.allowed_providers",
                            &format!("'{name}' is not a known provider"),
                        )));
                    }
                    names.push(name.to_string());
                }
                if names.is_empty() {
                    return Err(ManagedConfigError::PolicyField(field_error(
                        "policy.allowed_providers",
                        "must not be empty; omit the key to allow all providers",
                    )));
                }
                Some(names)
            }
        };
        let min_reasoning_effort = match policy.get("min_reasoning_effort") {
            None => None,
            Some(raw) => {
                let raw = raw.as_str().ok_or_else(|| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.min_reasoning_effort",
                        "must be a string naming an effort tier",
                    ))
                })?;
                let effort = ReasoningEffort::parse(raw).map_err(|_| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.min_reasoning_effort",
                        &format!(
                            "'{raw}' is not an effort tier; expected one of {}",
                            llm_router::REASONING_EFFORT_NAMES.join("|")
                        ),
                    ))
                })?;
                Some(effort)
            }
        };
        let max_permission_mode = match policy.get("max_permission_mode") {
            None => None,
            Some(raw) => {
                let raw = raw.as_str().ok_or_else(|| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.max_permission_mode",
                        "must be a string naming a permission mode",
                    ))
                })?;
                let mode = crate::permissions::PermissionMode::parse(raw).ok_or_else(|| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.max_permission_mode",
                        &format!(
                            "'{raw}' is not a permission mode; expected one of {}",
                            crate::permissions::MODE_NAMES.join(", ")
                        ),
                    ))
                })?;
                Some(mode)
            }
        };
        let denied_tools = match policy.get("denied_tools") {
            None => None,
            Some(raw) => {
                let entries = raw.as_array().ok_or_else(|| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.denied_tools",
                        "must be an array of tool names (optionally 'name(arg-glob)')",
                    ))
                })?;
                let mut patterns = Vec::with_capacity(entries.len());
                for entry in entries {
                    let name = entry.as_str().ok_or_else(|| {
                        ManagedConfigError::PolicyField(field_error(
                            "policy.denied_tools",
                            "entries must be strings",
                        ))
                    })?;
                    let pattern = crate::permissions::ToolPattern::parse(name).ok_or_else(|| {
                        ManagedConfigError::PolicyField(field_error(
                            "policy.denied_tools",
                            &format!("'{name}' is not a valid tool pattern"),
                        ))
                    })?;
                    patterns.push(pattern);
                }
                if patterns.is_empty() {
                    return Err(ManagedConfigError::PolicyField(field_error(
                        "policy.denied_tools",
                        "must not be empty; omit the key to ban nothing",
                    )));
                }
                Some(patterns)
            }
        };
        let confine_writes_to = match policy.get("confine_writes_to") {
            None => None,
            Some(raw) => {
                let raw = raw.as_str().ok_or_else(|| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.confine_writes_to",
                        "must be a workspace-relative path string",
                    ))
                })?;
                // Reuse the same relative/no-traversal validation every
                // other workspace-relative path in this codebase goes
                // through, rather than a second, possibly-divergent check.
                let repo_path = protocol::RepoPath::parse(raw).map_err(|_| {
                    ManagedConfigError::PolicyField(field_error(
                        "policy.confine_writes_to",
                        "must be a workspace-relative path with no '..' or absolute segments",
                    ))
                })?;
                Some(repo_path.as_str().to_owned())
            }
        };
        Ok(Self {
            locked_default,
            allowed_providers,
            min_reasoning_effort,
            max_permission_mode,
            denied_tools,
            confine_writes_to,
        })
    }

    pub fn locked_default(&self) -> Option<&str> {
        self.locked_default.as_deref()
    }

    pub fn allowed_providers(&self) -> Option<&[String]> {
        self.allowed_providers.as_deref()
    }

    pub fn min_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.min_reasoning_effort
    }

    pub fn max_permission_mode(&self) -> Option<crate::permissions::PermissionMode> {
        self.max_permission_mode
    }

    pub fn denied_tools(&self) -> Option<&[crate::permissions::ToolPattern]> {
        self.denied_tools.as_deref()
    }

    pub fn confine_writes_to(&self) -> Option<&str> {
        self.confine_writes_to.as_deref()
    }
}

/// Gate a resolved permission mode against the managed ceiling: only ever
/// narrows what mode resolution already decided, never widens it. Mirrors
/// `min_reasoning_effort`'s silent-enforcement shape (report, don't refuse
/// the whole run) — automatically becoming *more* restrictive is always
/// safe, unlike a model misconfiguration that could break every request.
pub fn gate_permission_mode(
    mode: crate::permissions::PermissionMode,
    policy: Option<&ManagedPolicy>,
) -> (crate::permissions::PermissionMode, Option<GateReportEntry>) {
    let Some(ceiling) = policy.and_then(ManagedPolicy::max_permission_mode) else {
        return (mode, None);
    };
    if mode.permissiveness_rank() <= ceiling.permissiveness_rank() {
        return (mode, None);
    }
    let report = GateReportEntry {
        field_id: "permission.mode".to_string(),
        origin: ConfigOrigin::Managed,
        detail: format!(
            "mode lowered from '{}' to the managed ceiling '{}'",
            mode.as_str(),
            ceiling.as_str()
        ),
        remediation: "contact your administrator to raise the managed permission-mode ceiling",
    };
    (ceiling, Some(report))
}

fn field_error(field_id: &str, reason: &str) -> ConfigFieldError {
    ConfigFieldError {
        field_id: field_id.to_string(),
        origin: ConfigOrigin::Managed,
        reason: reason.to_string(),
        remediation: "fix the managed policy document",
    }
}

/// Outcome of gating one resolution.
#[derive(Clone, Debug, PartialEq)]
pub struct GatedResolution {
    pub active: ActiveModel,
    /// The layer that decided the default model.
    pub default_origin: ConfigOrigin,
    /// Enforced gates, for the operator report.
    pub reports: Vec<GateReportEntry>,
}

/// One enforced gate, reported with provenance and remediation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateReportEntry {
    pub field_id: String,
    pub origin: ConfigOrigin,
    pub detail: String,
    pub remediation: &'static str,
}

impl fmt::Display for GateReportEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} enforced at '{}' (origin={}); remediation: {}",
            self.field_id, self.detail, self.origin.as_str(), self.remediation
        )
    }
}

/// Combined selection failure: user-config errors and managed-layer errors.
#[derive(Debug)]
pub enum GatedConfigError {
    Config(UserConfigError),
    Managed(ManagedConfigError),
    /// A user field violated a managed gate; the turn is refused.
    Field(ConfigFieldError),
}

impl fmt::Display for GatedConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(err) => write!(f, "{err}"),
            Self::Managed(err) => write!(f, "{err}"),
            Self::Field(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for GatedConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(err) => Some(err),
            Self::Managed(err) => Some(err),
            Self::Field(err) => Some(err),
        }
    }
}

impl From<UserConfigError> for GatedConfigError {
    fn from(value: UserConfigError) -> Self {
        Self::Config(value)
    }
}

impl From<ManagedConfigError> for GatedConfigError {
    fn from(value: ManagedConfigError) -> Self {
        Self::Managed(value)
    }
}

/// Load the managed policy named by the environment. Unset means no policy;
/// set-but-unreadable is an error (a configured-but-absent control document
/// must not silently become "no policy").
pub fn load_policy(env: &[(String, String)]) -> Result<Option<ManagedPolicy>, ManagedConfigError> {
    let Some(path) = env_value(env, MANAGED_CONFIG_ENV).map(str::trim).filter(|p| !p.is_empty())
    else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path)?;
    ManagedPolicy::parse(&text).map(Some)
}

/// Resolve the active model under the managed policy. The resolution order
/// for the default model becomes managed > env > user; provider allowlist
/// violations refuse the turn; the effort floor is enforced by raising.
pub fn resolve_gated(
    env: &[(String, String)],
    config: &UserConfig,
    policy: Option<&ManagedPolicy>,
) -> Result<GatedResolution, GatedConfigError> {
    let Some(policy) = policy else {
        let active = resolve_active(env, config)?;
        let default_origin = if env_value(env, DEFAULT_MODEL_ENV).is_some() {
            ConfigOrigin::Env
        } else if config.models.default.is_some() {
            ConfigOrigin::User
        } else {
            ConfigOrigin::Default
        };
        return Ok(GatedResolution {
            active,
            default_origin,
            reports: Vec::new(),
        });
    };

    // Managed default: re-resolve with the lock in force and the env
    // override removed, so managed > env > user holds for the whole
    // resolution (including `[phases]` targets).
    let mut reports = Vec::new();
    let effective_env: Vec<(String, String)> = env
        .iter()
        .filter(|(key, _)| key != DEFAULT_MODEL_ENV)
        .cloned()
        .collect();
    let active = if let Some(locked) = policy.locked_default.as_deref() {
        let mut gated_config = config.clone();
        gated_config.models.default = Some(locked.to_string());
        let locked_active = resolve_active(&effective_env, &gated_config)?;
        if locked_active.profile_id != active_profile_id(env, config)? {
            reports.push(GateReportEntry {
                field_id: "models.default".to_string(),
                origin: ConfigOrigin::Managed,
                detail: format!("default locked to '{locked}'"),
                remediation: "contact your administrator to change the default model",
            });
        }
        locked_active
    } else {
        resolve_active(env, config)?
    };

    // Provider allowlist applies to the final active model, whoever chose it.
    if let Some(allowed) = &policy.allowed_providers
        && !allowed.iter().any(|name| name == active.entry.provider.as_str())
    {
        return Err(GatedConfigError::Field(ConfigFieldError {
            field_id: format!("{}.provider", active.profile_id),
            origin: ConfigOrigin::User,
            reason: format!(
                "provider '{}' is not on the managed allowlist ({})",
                active.entry.provider.as_str(),
                allowed.join(", ")
            ),
            remediation: "pick an allowed provider or ask your administrator to allow this one",
        }));
    }

    // Effort floor: raising is enforcement, reported with provenance.
    if let Some(floor) = policy.min_reasoning_effort {
        let raised = active.entry.reasoning_effort.is_none_or(|current| current < floor);
        if raised {
            let mut gated = active;
            gated.entry.reasoning_effort = Some(floor);
            reports.push(GateReportEntry {
                field_id: format!("{}.reasoning_effort", gated.profile_id),
                origin: ConfigOrigin::Managed,
                detail: format!("reasoning effort raised to '{}'", floor.name()),
                remediation: "configure an effort at or above the managed floor",
            });
            return finish(gated, env, config, policy, reports);
        }
    }

    finish(active, env, config, policy, reports)
}

fn active_profile_id(env: &[(String, String)], config: &UserConfig) -> Result<String, GatedConfigError> {
    Ok(resolve_active(env, config)?.profile_id)
}

fn finish(
    active: ActiveModel,
    env: &[(String, String)],
    config: &UserConfig,
    policy: &ManagedPolicy,
    reports: Vec<GateReportEntry>,
) -> Result<GatedResolution, GatedConfigError> {
    let default_origin = if policy.locked_default.is_some() {
        ConfigOrigin::Managed
    } else if env_value(env, DEFAULT_MODEL_ENV).is_some() {
        ConfigOrigin::Env
    } else if config.models.default.is_some() {
        ConfigOrigin::User
    } else {
        ConfigOrigin::Default
    };
    Ok(GatedResolution {
        active,
        default_origin,
        reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::parse_config_document;

    fn user_doc() -> &'static str {
        r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"

[model.cloud]
provider = "anthropic"
model = "claude-3-5-sonnet"
base_url = "http://gateway.internal:8080"
"#
    }

    fn parse_policy(toml: &str) -> ManagedPolicy {
        ManagedPolicy::parse(toml).expect("parse policy")
    }

    fn policy_doc(body: &str) -> String {
        format!("schema = \"{MANAGED_SCHEMA}\"\n[policy]\n{body}")
    }

    #[test]
    fn parse_reads_all_policy_fields_and_rejects_unknowns() {
        let policy = parse_policy(&policy_doc(
            "locked_default = \"cloud\"\nallowed_providers = [\"anthropic\"]\nmin_reasoning_effort = \"high\"\nmax_permission_mode = \"acceptEdits\"\ndenied_tools = [\"shell_exec\"]\nconfine_writes_to = \"src\"\n",
        ));
        assert_eq!(policy.locked_default(), Some("cloud"));
        assert_eq!(
            policy.allowed_providers(),
            Some(["anthropic".to_owned()].as_slice())
        );
        assert_eq!(policy.min_reasoning_effort(), Some(ReasoningEffort::High));
        assert_eq!(
            policy.max_permission_mode(),
            Some(crate::permissions::PermissionMode::AcceptEdits)
        );
        assert_eq!(
            policy.denied_tools(),
            Some([crate::permissions::ToolPattern::parse("shell_exec").expect("pattern")].as_slice())
        );
        assert_eq!(policy.confine_writes_to(), Some("src"));
        let bad = format!("schema = \"{MANAGED_SCHEMA}\"\nsurprise = 1\n[policy]\n");
        let err = ManagedPolicy::parse(&bad).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field 'surprise'"));
        let wrong_schema = "schema = \"rapidlm.managed_config.v0\"\n[policy]\n";
        let err = ManagedPolicy::parse(wrong_schema).expect_err("schema");
        assert!(matches!(err, ManagedConfigError::SchemaMismatch { .. }));
    }

    #[test]
    fn parse_rejects_an_empty_allowlist_as_a_policy_authoring_error() {
        let err = ManagedPolicy::parse(&policy_doc("allowed_providers = []\n"))
            .expect_err("empty allowlist");
        assert!(err.to_string().contains("must not be empty"));
        let err = ManagedPolicy::parse(&policy_doc("allowed_providers = [\"smtp\"]\n"))
            .expect_err("unknown provider");
        assert!(err.to_string().contains("not a known provider"));
    }

    #[test]
    fn load_policy_reads_the_env_named_file() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-managed-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&dir, policy_doc("locked_default = \"cloud\"\n")).expect("write");
        let env = vec![(
            MANAGED_CONFIG_ENV.to_string(),
            dir.display().to_string(),
        )];
        let policy = load_policy(&env).expect("load");
        assert_eq!(policy.and_then(|p| p.locked_default().map(str::to_string)).as_deref(), Some("cloud"));
        // Unset: no policy.
        assert!(load_policy(&[]).expect("unset").is_none());
        // Set but absent: an error, not "no policy".
        let env = vec![(MANAGED_CONFIG_ENV.to_string(), "/nonexistent/rapidlm/managed.toml".to_string())];
        assert!(matches!(
            load_policy(&env),
            Err(ManagedConfigError::Io(_))
        ));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn no_policy_preserves_user_and_env_resolution() {
        let config = parse_config_document(user_doc(), "user.toml").expect("parse");
        let gated = resolve_gated(&[], &config, None).expect("gated");
        assert_eq!(gated.active.profile_id, "local");
        assert_eq!(gated.default_origin, ConfigOrigin::User);
        assert!(gated.reports.is_empty());
        let env = vec![(DEFAULT_MODEL_ENV.to_string(), "cloud".to_string())];
        let gated = resolve_gated(&env, &config, None).expect("gated");
        assert_eq!(gated.active.profile_id, "cloud");
        assert_eq!(gated.default_origin, ConfigOrigin::Env);
    }

    #[test]
    fn locked_default_beats_env_and_user_selection() {
        let config = parse_config_document(user_doc(), "user.toml").expect("parse");
        let policy = parse_policy(&policy_doc("locked_default = \"cloud\"\n"));
        // Env override loses to the lock.
        let env = vec![(DEFAULT_MODEL_ENV.to_string(), "local".to_string())];
        let gated = resolve_gated(&env, &config, Some(&policy)).expect("gated");
        assert_eq!(gated.active.profile_id, "cloud");
        assert_eq!(gated.default_origin, ConfigOrigin::Managed);
        assert!(gated.reports.iter().any(|r| r.field_id == "models.default"));
        // An already-compliant selection (user default == lock) reports no
        // override — the layer that decided is still the managed lock, but
        // nothing changed.
        let compliant_doc = user_doc().replace("default = \"local\"", "default = \"cloud\"");
        let compliant = parse_config_document(&compliant_doc, "user.toml").expect("parse");
        let gated = resolve_gated(&[], &compliant, Some(&policy)).expect("gated");
        assert_eq!(gated.active.profile_id, "cloud");
        assert!(gated.reports.iter().all(|r| r.field_id != "models.default"));
    }

    #[test]
    fn allowlist_violation_refuses_the_turn_with_field_provenance() {
        let config = parse_config_document(user_doc(), "user.toml").expect("parse");
        let policy = parse_policy(&policy_doc("allowed_providers = [\"anthropic\"]\n"));
        let err = resolve_gated(&[], &config, Some(&policy)).expect_err("violation");
        match err {
            GatedConfigError::Field(field) => {
                assert_eq!(field.field_id, "local.provider");
                assert_eq!(field.origin, ConfigOrigin::User);
                assert!(field.reason.contains("openai-compatible"));
                assert!(!field.remediation.is_empty());
            }
            other => panic!("expected Field violation, got {other:?}"),
        }
        // The locked default passing the allowlist is fine.
        let policy = parse_policy(&policy_doc(
            "locked_default = \"cloud\"\nallowed_providers = [\"anthropic\"]\n",
        ));
        let gated = resolve_gated(&[], &config, Some(&policy)).expect("gated");
        assert_eq!(gated.active.profile_id, "cloud");
    }

    #[test]
    fn effort_floor_raises_configured_and_unset_efforts() {
        let doc = r#"
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"
reasoning_effort = "low"
"#;
        let config = parse_config_document(doc, "user.toml").expect("parse");
        let policy = parse_policy(&policy_doc("min_reasoning_effort = \"high\"\n"));
        let gated = resolve_gated(&[], &config, Some(&policy)).expect("gated");
        assert_eq!(gated.active.entry.reasoning_effort, Some(ReasoningEffort::High));
        assert!(gated.reports.iter().any(|r| r.field_id == "local.reasoning_effort"));
        // An effort already at or above the floor is left alone.
        let doc = doc.replace("reasoning_effort = \"low\"", "reasoning_effort = \"ultra\"");
        let config = parse_config_document(&doc, "user.toml").expect("parse");
        let gated = resolve_gated(&[], &config, Some(&policy)).expect("gated");
        assert_eq!(gated.active.entry.reasoning_effort, Some(ReasoningEffort::Ultra));
        assert!(gated.reports.is_empty());
    }

    #[test]
    fn parse_rejects_an_unknown_permission_mode_name() {
        let err = ManagedPolicy::parse(&policy_doc("max_permission_mode = \"godmode\"\n"))
            .expect_err("unknown mode");
        assert!(err.to_string().contains("not a permission mode"));
    }

    #[test]
    fn parse_rejects_an_invalid_denied_tools_pattern() {
        let err = ManagedPolicy::parse(&policy_doc("denied_tools = [\"\"]\n")).expect_err("bad pattern");
        assert!(err.to_string().contains("not a valid tool pattern"));
    }

    #[test]
    fn parse_rejects_an_empty_denied_tools_array() {
        let err = ManagedPolicy::parse(&policy_doc("denied_tools = []\n")).expect_err("empty");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn parse_rejects_an_absolute_or_traversing_confine_writes_to() {
        let absolute = ManagedPolicy::parse(&policy_doc("confine_writes_to = \"/etc\"\n"))
            .expect_err("absolute path");
        assert!(absolute.to_string().contains("no '..' or absolute segments"));

        let traversal = ManagedPolicy::parse(&policy_doc("confine_writes_to = \"../outside\"\n"))
            .expect_err("traversal");
        assert!(traversal.to_string().contains("no '..' or absolute segments"));
    }

    #[test]
    fn gate_permission_mode_only_ever_narrows_never_widens() {
        use crate::permissions::PermissionMode;
        let policy = parse_policy(&policy_doc("max_permission_mode = \"acceptEdits\"\n"));

        // Requesting more than the ceiling: lowered, and reported.
        let (mode, report) = gate_permission_mode(PermissionMode::BypassPermissions, Some(&policy));
        assert_eq!(mode, PermissionMode::AcceptEdits);
        let report = report.expect("must be reported");
        assert_eq!(report.field_id, "permission.mode");
        assert_eq!(report.origin, ConfigOrigin::Managed);

        // At or under the ceiling: passed through untouched, no report.
        let (mode, report) = gate_permission_mode(PermissionMode::Default, Some(&policy));
        assert_eq!(mode, PermissionMode::Default);
        assert!(report.is_none());
        let (mode, report) = gate_permission_mode(PermissionMode::AcceptEdits, Some(&policy));
        assert_eq!(mode, PermissionMode::AcceptEdits);
        assert!(report.is_none());

        // Plan is the strictest mode of all six — always passes through
        // even against the tightest possible ceiling.
        let strict = parse_policy(&policy_doc("max_permission_mode = \"plan\"\n"));
        let (mode, report) = gate_permission_mode(PermissionMode::Plan, Some(&strict));
        assert_eq!(mode, PermissionMode::Plan);
        assert!(report.is_none());

        // No policy at all: never gates.
        let (mode, report) = gate_permission_mode(PermissionMode::BypassPermissions, None);
        assert_eq!(mode, PermissionMode::BypassPermissions);
        assert!(report.is_none());
    }
}
