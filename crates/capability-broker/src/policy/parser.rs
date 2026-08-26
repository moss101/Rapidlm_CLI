//! Typed TOML policy parser.
//!
//! `PolicyDocument::parse_toml` compiles a declarative document into ordered
//! rules stamped with caller-supplied source trust. The file cannot claim a
//! higher layer. Unknown privileged match fields fail closed. Workspace /
//! trusted-project documents may only restrict (`deny` / `ask`).

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::capability::{
    Capability, CapabilityError, CapabilityFamily, CommandFamily, DeviceId, FilesystemRoot,
    GitRefScope, Hostname, NetworkScheme, Origin, PathGlob, SecretHandle, SecretUseTarget,
};
use crate::normalize::command::CancellationToken;

/// Wire schema version for policy documents.
pub const POLICY_SCHEMA: u16 = 1;

/// Maximum UTF-8 bytes accepted in one policy document.
pub const MAX_POLICY_DOCUMENT_BYTES: usize = 256 * 1024;

/// Maximum rules in one document.
pub const MAX_POLICY_RULES: usize = 256;

/// Maximum subjects on one rule.
pub const MAX_SUBJECTS: usize = 32;

/// Maximum UTF-8 bytes in a rule id.
pub const MAX_RULE_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes in a caller-supplied origin label.
pub const MAX_ORIGIN_BYTES: usize = 256;

/// Maximum UTF-8 bytes in one subject token.
pub const MAX_SUBJECT_BYTES: usize = 128;

const CANCEL_STRIDE: usize = 16;
const DOCUMENT_FIELDS: &[&str] = &["schema", "rules"];
const RULE_FIELDS: &[&str] = &[
    "id",
    "effect",
    "subjects",
    "capability",
    "resource",
    "conditions",
];
const FS_RESOURCE_FIELDS: &[&str] = &["root", "glob"];
const PROC_RESOURCE_FIELDS: &[&str] = &["command_family"];
const NET_RESOURCE_FIELDS: &[&str] = &["scheme", "host", "port"];
const GIT_RESOURCE_FIELDS: &[&str] = &["ref_scope"];
const SECRET_RESOURCE_FIELDS: &[&str] = &["secret_id", "target"];
const BROWSER_RESOURCE_FIELDS: &[&str] = &["origin", "path"];
const MOBILE_RESOURCE_FIELDS: &[&str] = &["device_id"];
const MCP_RESOURCE_FIELDS: &[&str] = &["server", "tool"];
const PLUGIN_RESOURCE_FIELDS: &[&str] = &["plugin", "capability"];

/// High-to-low trust policy layer (SDD 11.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum PolicyLayer {
    Compiled,
    Organization,
    User,
    TrustedProject,
    Session,
}

/// Caller-supplied origin and trust. Debug omits nothing secret: labels only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicySource {
    layer: PolicyLayer,
    origin: String,
}

/// `allow` / `ask` / `deny`. Deny at a higher-trust layer is final.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PolicyEffect {
    Allow,
    Ask,
    Deny,
}

/// Stable rule identity used by `policy explain`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RuleId(String);

/// Subject matcher. `*` is the only wildcard and means every principal.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum SubjectPattern {
    Any,
    Named(String),
}

/// Closed resource matcher. Omitted document `resource` becomes [`ResourcePattern::Any`].
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ResourcePattern {
    Any,
    Filesystem {
        root: Option<FilesystemRoot>,
        glob: Option<PathGlob>,
    },
    Process {
        command_family: Option<CommandFamily>,
    },
    Network {
        scheme: Option<NetworkScheme>,
        host: Option<Hostname>,
        port: Option<u16>,
    },
    Git {
        ref_scope: Option<GitRefScope>,
    },
    Secret {
        secret_id: Option<SecretHandle>,
        target: Option<SecretUseTarget>,
    },
    Browser {
        origin: Option<Origin>,
        path: Option<PathGlob>,
    },
    Mobile {
        device_id: Option<DeviceId>,
    },
    Mcp {
        server: Option<String>,
        tool: Option<String>,
    },
    Plugin {
        plugin: Option<String>,
        capability: Option<String>,
    },
}

/// v1 has no condition keys. A non-empty table is an unknown privileged field.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct RuleConditions;

/// One compiled rule. `id` is preserved for explain/audit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyRule {
    id: RuleId,
    effect: PolicyEffect,
    subjects: Vec<SubjectPattern>,
    capability_pattern: Capability,
    resource_pattern: ResourcePattern,
    conditions: RuleConditions,
    source: PolicySource,
}

/// Parsed document: document order + source trust.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyDocument {
    schema: u16,
    source: PolicySource,
    rules: Vec<PolicyRule>,
}

/// Field that failed construction or decode. Names only; never values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PolicyField {
    Schema,
    Rules,
    Id,
    Effect,
    Subjects,
    Capability,
    Resource,
    Conditions,
    Origin,
}

/// Typed parse failure. Display never echoes document values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyParseError {
    Cancelled,
    SourceTooLarge,
    InvalidSyntax,
    UnsupportedSchema { found: u16 },
    MissingField { field: PolicyField },
    UnknownPrivilegedField { field: String },
    InvalidValue { field: PolicyField },
    DuplicateRuleId,
    TooManyRules,
    TooManySubjects,
    Broadening { field: PolicyField },
    Capability(CapabilityError),
}

impl PolicyLayer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compiled => "compiled",
            Self::Organization => "organization",
            Self::User => "user",
            Self::TrustedProject => "trusted_project",
            Self::Session => "session",
        }
    }

    /// Compiled / org / user may grant. Project and session may only restrict.
    pub const fn may_grant(self) -> bool {
        matches!(self, Self::Compiled | Self::Organization | Self::User)
    }
}

impl PolicySource {
    pub fn compiled() -> Self {
        Self {
            layer: PolicyLayer::Compiled,
            origin: "compiled".to_owned(),
        }
    }

    pub fn organization(origin: &str) -> Result<Self, PolicyParseError> {
        Self::new(PolicyLayer::Organization, origin)
    }

    pub fn user(origin: &str) -> Result<Self, PolicyParseError> {
        Self::new(PolicyLayer::User, origin)
    }

    pub fn trusted_project(origin: &str) -> Result<Self, PolicyParseError> {
        Self::new(PolicyLayer::TrustedProject, origin)
    }

    pub fn session(origin: &str) -> Result<Self, PolicyParseError> {
        Self::new(PolicyLayer::Session, origin)
    }

    pub fn new(layer: PolicyLayer, origin: &str) -> Result<Self, PolicyParseError> {
        let origin = parse_origin(origin)?;
        Ok(Self { layer, origin })
    }

    pub fn layer(&self) -> PolicyLayer {
        self.layer
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Source trust is the layer. Files cannot raise this.
    pub fn trust(&self) -> PolicyLayer {
        self.layer
    }
}

impl PolicyEffect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

impl RuleId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SubjectPattern {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Any => "*",
            Self::Named(name) => name,
        }
    }
}

impl PolicyRule {
    pub fn id(&self) -> &RuleId {
        &self.id
    }

    pub fn effect(&self) -> PolicyEffect {
        self.effect
    }

    pub fn subjects(&self) -> &[SubjectPattern] {
        &self.subjects
    }

    pub fn capability_pattern(&self) -> Capability {
        self.capability_pattern
    }

    pub fn resource_pattern(&self) -> &ResourcePattern {
        &self.resource_pattern
    }

    pub fn conditions(&self) -> &RuleConditions {
        &self.conditions
    }

    pub fn source(&self) -> &PolicySource {
        &self.source
    }
}

impl PolicyDocument {
    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn source(&self) -> &PolicySource {
        &self.source
    }

    /// Layer/trust of the document. Copied onto every rule.
    pub fn source_trust(&self) -> PolicyLayer {
        self.source.trust()
    }

    /// Rules in document order.
    pub fn rules(&self) -> &[PolicyRule] {
        &self.rules
    }

    /// Parse a TOML policy file. `source` is caller-attested trust/provenance.
    pub fn parse_toml(
        src: &str,
        source: PolicySource,
        cancel: &CancellationToken,
    ) -> Result<Self, PolicyParseError> {
        if cancel.is_cancelled() {
            return Err(PolicyParseError::Cancelled);
        }
        if src.len() > MAX_POLICY_DOCUMENT_BYTES {
            return Err(PolicyParseError::SourceTooLarge);
        }
        if src.bytes().all(|b| b.is_ascii_whitespace()) {
            return Ok(Self {
                schema: POLICY_SCHEMA,
                source,
                rules: Vec::new(),
            });
        }

        let parsed: toml::Value =
            toml::from_str(src).map_err(|_| PolicyParseError::InvalidSyntax)?;
        let table = parsed.as_table().ok_or(PolicyParseError::InvalidSyntax)?;
        reject_unknown_keys(table, DOCUMENT_FIELDS, "")?;

        let schema = match table.get("schema") {
            None => POLICY_SCHEMA,
            Some(value) => parse_schema(value)?,
        };

        let rules = match table.get("rules") {
            None => Vec::new(),
            Some(value) => parse_rules(value, &source, cancel)?,
        };

        Ok(Self {
            schema,
            source,
            rules,
        })
    }
}

impl PolicyField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Rules => "rules",
            Self::Id => "id",
            Self::Effect => "effect",
            Self::Subjects => "subjects",
            Self::Capability => "capability",
            Self::Resource => "resource",
            Self::Conditions => "conditions",
            Self::Origin => "origin",
        }
    }
}

impl fmt::Display for PolicyLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for PolicyEffect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for PolicyField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for PolicyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("policy parse cancelled"),
            Self::SourceTooLarge => f.write_str("policy document exceeds size bound"),
            Self::InvalidSyntax => f.write_str("invalid policy syntax"),
            Self::UnsupportedSchema { found } => {
                write!(f, "unsupported policy schema {found}")
            }
            Self::MissingField { field } => write!(f, "missing policy field {field}"),
            Self::UnknownPrivilegedField { field } => {
                write!(f, "unknown privileged policy field {field}")
            }
            Self::InvalidValue { field } => write!(f, "invalid policy value for {field}"),
            Self::DuplicateRuleId => f.write_str("duplicate policy rule id"),
            Self::TooManyRules => f.write_str("too many policy rules"),
            Self::TooManySubjects => f.write_str("too many policy subjects"),
            Self::Broadening { field } => {
                write!(f, "workspace policy cannot broaden {field}")
            }
            Self::Capability(err) => write!(f, "policy capability: {err}"),
        }
    }
}

impl Error for PolicyParseError {}

fn parse_origin(origin: &str) -> Result<String, PolicyParseError> {
    if origin.is_empty() {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Origin,
        });
    }
    if origin.len() > MAX_ORIGIN_BYTES {
        return Err(PolicyParseError::SourceTooLarge);
    }
    if origin.contains('\0') || origin.chars().any(char::is_control) {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Origin,
        });
    }
    Ok(origin.to_owned())
}

fn parse_schema(value: &toml::Value) -> Result<u16, PolicyParseError> {
    let found = integer_u64(value, PolicyField::Schema)?;
    if found > u64::from(u16::MAX) {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Schema,
        });
    }
    let found = found as u16;
    if found == POLICY_SCHEMA {
        Ok(found)
    } else {
        Err(PolicyParseError::UnsupportedSchema { found })
    }
}

fn parse_rules(
    value: &toml::Value,
    source: &PolicySource,
    cancel: &CancellationToken,
) -> Result<Vec<PolicyRule>, PolicyParseError> {
    let items = value.as_array().ok_or(PolicyParseError::InvalidValue {
        field: PolicyField::Rules,
    })?;
    if items.len() > MAX_POLICY_RULES {
        return Err(PolicyParseError::TooManyRules);
    }

    let mut rules = Vec::with_capacity(items.len());
    let mut seen_ids = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        if index % CANCEL_STRIDE == 0 && cancel.is_cancelled() {
            return Err(PolicyParseError::Cancelled);
        }
        let table = item.as_table().ok_or(PolicyParseError::InvalidValue {
            field: PolicyField::Rules,
        })?;
        let rule = parse_rule(table, source)?;
        if !seen_ids.insert(rule.id.as_str().to_owned()) {
            return Err(PolicyParseError::DuplicateRuleId);
        }
        rules.push(rule);
    }
    Ok(rules)
}

fn parse_rule(
    table: &toml::map::Map<String, toml::Value>,
    source: &PolicySource,
) -> Result<PolicyRule, PolicyParseError> {
    reject_unknown_keys(table, RULE_FIELDS, "rules")?;

    let id = parse_rule_id(require_value(table, PolicyField::Id)?)?;
    let effect = parse_effect(require_value(table, PolicyField::Effect)?)?;
    if effect == PolicyEffect::Allow && !source.layer.may_grant() {
        return Err(PolicyParseError::Broadening {
            field: PolicyField::Effect,
        });
    }

    let subjects = match table.get("subjects") {
        None => vec![SubjectPattern::Any],
        Some(value) => parse_subjects(value)?,
    };
    let capability_pattern = parse_capability(require_value(table, PolicyField::Capability)?)?;
    let resource_pattern = match table.get("resource") {
        None => ResourcePattern::Any,
        Some(value) => parse_resource(value, capability_pattern.family())?,
    };
    let conditions = match table.get("conditions") {
        None => RuleConditions,
        Some(value) => parse_conditions(value)?,
    };

    Ok(PolicyRule {
        id,
        effect,
        subjects,
        capability_pattern,
        resource_pattern,
        conditions,
        source: source.clone(),
    })
}

fn parse_rule_id(value: &toml::Value) -> Result<RuleId, PolicyParseError> {
    let raw = value.as_str().ok_or(PolicyParseError::InvalidValue {
        field: PolicyField::Id,
    })?;
    if raw.is_empty() || raw.len() > MAX_RULE_ID_BYTES {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Id,
        });
    }
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Id,
        });
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Id,
        });
    }
    Ok(RuleId(raw.to_owned()))
}

fn parse_effect(value: &toml::Value) -> Result<PolicyEffect, PolicyParseError> {
    let raw = value.as_str().ok_or(PolicyParseError::InvalidValue {
        field: PolicyField::Effect,
    })?;
    match raw {
        "allow" => Ok(PolicyEffect::Allow),
        "ask" => Ok(PolicyEffect::Ask),
        "deny" => Ok(PolicyEffect::Deny),
        _ => Err(PolicyParseError::InvalidValue {
            field: PolicyField::Effect,
        }),
    }
}

fn parse_subjects(value: &toml::Value) -> Result<Vec<SubjectPattern>, PolicyParseError> {
    let items = value.as_array().ok_or(PolicyParseError::InvalidValue {
        field: PolicyField::Subjects,
    })?;
    if items.is_empty() {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Subjects,
        });
    }
    if items.len() > MAX_SUBJECTS {
        return Err(PolicyParseError::TooManySubjects);
    }
    let mut subjects = Vec::with_capacity(items.len());
    for item in items {
        let raw = item.as_str().ok_or(PolicyParseError::InvalidValue {
            field: PolicyField::Subjects,
        })?;
        subjects.push(parse_subject(raw)?);
    }
    Ok(subjects)
}

fn parse_subject(raw: &str) -> Result<SubjectPattern, PolicyParseError> {
    if raw == "*" {
        return Ok(SubjectPattern::Any);
    }
    if raw.is_empty() || raw.len() > MAX_SUBJECT_BYTES {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Subjects,
        });
    }
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Subjects,
        });
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/'))
    {
        return Err(PolicyParseError::InvalidValue {
            field: PolicyField::Subjects,
        });
    }
    Ok(SubjectPattern::Named(raw.to_owned()))
}

fn parse_capability(value: &toml::Value) -> Result<Capability, PolicyParseError> {
    let raw = value.as_str().ok_or(PolicyParseError::InvalidValue {
        field: PolicyField::Capability,
    })?;
    raw.parse().map_err(PolicyParseError::Capability)
}

fn parse_resource(
    value: &toml::Value,
    family: CapabilityFamily,
) -> Result<ResourcePattern, PolicyParseError> {
    let table = value.as_table().ok_or(PolicyParseError::InvalidValue {
        field: PolicyField::Resource,
    })?;
    let allowed = resource_fields(family);
    reject_unknown_keys(table, allowed, "resource")?;
    match family {
        CapabilityFamily::Fs => {
            let root = optional_str(table, "root", PolicyField::Resource)?
                .map(|raw| raw.parse().map_err(PolicyParseError::Capability))
                .transpose()?;
            let glob = match (root, optional_str(table, "glob", PolicyField::Resource)?) {
                (_, None) => None,
                (Some(FilesystemRoot::Repo) | None, Some(raw)) => {
                    Some(PathGlob::repo(raw).map_err(PolicyParseError::Capability)?)
                }
                (Some(FilesystemRoot::Host), Some(raw)) => {
                    Some(PathGlob::host(raw).map_err(PolicyParseError::Capability)?)
                }
            };
            Ok(ResourcePattern::Filesystem { root, glob })
        }
        CapabilityFamily::Proc => Ok(ResourcePattern::Process {
            command_family: optional_str(table, "command_family", PolicyField::Resource)?
                .map(CommandFamily::parse)
                .transpose()
                .map_err(PolicyParseError::Capability)?,
        }),
        CapabilityFamily::Net => {
            let scheme = optional_str(table, "scheme", PolicyField::Resource)?
                .map(|raw| raw.parse().map_err(PolicyParseError::Capability))
                .transpose()?;
            let host = optional_str(table, "host", PolicyField::Resource)?
                .map(Hostname::parse)
                .transpose()
                .map_err(PolicyParseError::Capability)?;
            let port = optional_port(table)?;
            Ok(ResourcePattern::Network { scheme, host, port })
        }
        CapabilityFamily::Git => Ok(ResourcePattern::Git {
            ref_scope: optional_str(table, "ref_scope", PolicyField::Resource)?
                .map(GitRefScope::parse)
                .transpose()
                .map_err(PolicyParseError::Capability)?,
        }),
        CapabilityFamily::Secret => Ok(ResourcePattern::Secret {
            secret_id: optional_str(table, "secret_id", PolicyField::Resource)?
                .map(SecretHandle::parse)
                .transpose()
                .map_err(PolicyParseError::Capability)?,
            target: optional_str(table, "target", PolicyField::Resource)?
                .map(SecretUseTarget::parse)
                .transpose()
                .map_err(PolicyParseError::Capability)?,
        }),
        CapabilityFamily::Browser => {
            let origin = optional_str(table, "origin", PolicyField::Resource)?
                .map(Origin::parse)
                .transpose()
                .map_err(PolicyParseError::Capability)?;
            let path = optional_str(table, "path", PolicyField::Resource)?
                .map(|raw| {
                    PathGlob::repo(raw)
                        .or_else(|_| PathGlob::host(raw))
                        .map_err(PolicyParseError::Capability)
                })
                .transpose()?;
            Ok(ResourcePattern::Browser { origin, path })
        }
        CapabilityFamily::Mobile => Ok(ResourcePattern::Mobile {
            device_id: optional_str(table, "device_id", PolicyField::Resource)?
                .map(DeviceId::parse)
                .transpose()
                .map_err(PolicyParseError::Capability)?,
        }),
        CapabilityFamily::Mcp => Ok(ResourcePattern::Mcp {
            server: optional_ident(table, "server")?,
            tool: optional_ident(table, "tool")?,
        }),
        CapabilityFamily::Plugin => Ok(ResourcePattern::Plugin {
            plugin: optional_ident(table, "plugin")?,
            capability: optional_ident(table, "capability")?,
        }),
    }
}

fn parse_conditions(value: &toml::Value) -> Result<RuleConditions, PolicyParseError> {
    let table = value.as_table().ok_or(PolicyParseError::InvalidValue {
        field: PolicyField::Conditions,
    })?;
    reject_unknown_keys(table, &[], "conditions")?;
    Ok(RuleConditions)
}

fn resource_fields(family: CapabilityFamily) -> &'static [&'static str] {
    match family {
        CapabilityFamily::Fs => FS_RESOURCE_FIELDS,
        CapabilityFamily::Proc => PROC_RESOURCE_FIELDS,
        CapabilityFamily::Net => NET_RESOURCE_FIELDS,
        CapabilityFamily::Git => GIT_RESOURCE_FIELDS,
        CapabilityFamily::Secret => SECRET_RESOURCE_FIELDS,
        CapabilityFamily::Browser => BROWSER_RESOURCE_FIELDS,
        CapabilityFamily::Mobile => MOBILE_RESOURCE_FIELDS,
        CapabilityFamily::Mcp => MCP_RESOURCE_FIELDS,
        CapabilityFamily::Plugin => PLUGIN_RESOURCE_FIELDS,
    }
}

fn reject_unknown_keys(
    table: &toml::map::Map<String, toml::Value>,
    allowed: &[&str],
    prefix: &str,
) -> Result<(), PolicyParseError> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(PolicyParseError::UnknownPrivilegedField {
                field: field_path(prefix, key),
            });
        }
    }
    Ok(())
}

fn field_path(prefix: &str, key: &str) -> String {
    let safe = if key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        key
    } else {
        "invalid-key"
    };
    if prefix.is_empty() {
        safe.to_owned()
    } else {
        format!("{prefix}.{safe}")
    }
}

fn require_value(
    table: &toml::map::Map<String, toml::Value>,
    field: PolicyField,
) -> Result<&toml::Value, PolicyParseError> {
    table
        .get(field.as_str())
        .ok_or(PolicyParseError::MissingField { field })
}

fn optional_str<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
    field: PolicyField,
) -> Result<Option<&'a str>, PolicyParseError> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or(PolicyParseError::InvalidValue { field }),
    }
}

fn optional_ident(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<String>, PolicyParseError> {
    match optional_str(table, key, PolicyField::Resource)? {
        None => Ok(None),
        Some(raw) => {
            if raw.is_empty() || raw.len() > crate::capability::MAX_IDENT_BYTES {
                return Err(PolicyParseError::InvalidValue {
                    field: PolicyField::Resource,
                });
            }
            if raw.contains('\0') || raw.chars().any(char::is_control) {
                return Err(PolicyParseError::InvalidValue {
                    field: PolicyField::Resource,
                });
            }
            if !raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
            {
                return Err(PolicyParseError::InvalidValue {
                    field: PolicyField::Resource,
                });
            }
            Ok(Some(raw.to_owned()))
        }
    }
}

fn optional_port(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<Option<u16>, PolicyParseError> {
    match table.get("port") {
        None => Ok(None),
        Some(value) => {
            let port = integer_u64(value, PolicyField::Resource)?;
            if port == 0 || port > u64::from(u16::MAX) {
                return Err(PolicyParseError::InvalidValue {
                    field: PolicyField::Resource,
                });
            }
            Ok(Some(port as u16))
        }
    }
}

fn integer_u64(value: &toml::Value, field: PolicyField) -> Result<u64, PolicyParseError> {
    match value.as_integer() {
        Some(n) if n >= 0 => Ok(n as u64),
        _ => Err(PolicyParseError::InvalidValue { field }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "super-secret-password";

    fn parse(src: &str, source: PolicySource) -> Result<PolicyDocument, PolicyParseError> {
        PolicyDocument::parse_toml(src, source, &CancellationToken::new())
    }

    fn user() -> PolicySource {
        PolicySource::user("user-policy.toml").expect("user origin")
    }

    fn workspace() -> PolicySource {
        PolicySource::trusted_project(".rapidlm/policy.toml").expect("workspace origin")
    }

    fn allow_ask_deny_doc() -> &'static str {
        r#"
schema = 1

[[rules]]
id = "repo-read"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }

[[rules]]
id = "net-ask"
effect = "ask"
subjects = ["agent"]
capability = "net.connect"
resource = { scheme = "https", host = "example.com", port = 443 }

[[rules]]
id = "host-write-deny"
effect = "deny"
capability = "fs.write"
resource = { root = "host", glob = "/tmp/**" }
"#
    }

    #[test]
    fn parse_toml_returns_ordered_rules_and_source_trust() {
        let doc = parse(allow_ask_deny_doc(), user()).expect("parse");
        assert_eq!(doc.schema(), POLICY_SCHEMA);
        assert_eq!(doc.source_trust(), PolicyLayer::User);
        assert_eq!(doc.source().origin(), "user-policy.toml");
        assert_eq!(doc.rules().len(), 3);
        assert_eq!(
            doc.rules()
                .iter()
                .map(|rule| rule.id().as_str())
                .collect::<Vec<_>>(),
            ["repo-read", "net-ask", "host-write-deny"]
        );
        assert_eq!(doc.rules()[0].effect(), PolicyEffect::Allow);
        assert_eq!(doc.rules()[1].effect(), PolicyEffect::Ask);
        assert_eq!(doc.rules()[2].effect(), PolicyEffect::Deny);
        assert!(
            doc.rules()
                .iter()
                .all(|rule| rule.source().trust() == PolicyLayer::User)
        );
        assert_eq!(doc.rules()[0].capability_pattern(), Capability::FsRead);
        assert_eq!(doc.rules()[1].capability_pattern(), Capability::NetConnect);
        match doc.rules()[0].resource_pattern() {
            ResourcePattern::Filesystem {
                root: Some(FilesystemRoot::Repo),
                glob: Some(glob),
            } => assert_eq!(glob.as_str(), "src/**"),
            other => panic!("unexpected resource {other:?}"),
        }
    }

    #[test]
    fn workspace_deny_and_ask_are_accepted() {
        let src = r#"
[[rules]]
id = "tighten-net"
effect = "ask"
capability = "net.connect"

[[rules]]
id = "deny-host-write"
effect = "deny"
capability = "fs.write"
resource = { root = "host" }
"#;
        let doc = parse(src, workspace()).expect("restrictive workspace");
        assert_eq!(doc.source_trust(), PolicyLayer::TrustedProject);
        assert_eq!(doc.rules()[0].id().as_str(), "tighten-net");
        assert_eq!(doc.rules()[1].effect(), PolicyEffect::Deny);
    }

    #[test]
    fn workspace_allow_is_broadening_error() {
        let src = r#"
[[rules]]
id = "grant-net"
effect = "allow"
capability = "net.connect"
"#;
        assert_eq!(
            parse(src, workspace()),
            Err(PolicyParseError::Broadening {
                field: PolicyField::Effect,
            })
        );
        let err = parse(src, workspace()).expect_err("broadening");
        let text = err.to_string();
        assert!(text.contains("broaden"));
        assert!(!text.contains("grant-net"));
        assert!(!text.contains("net.connect"));
    }

    #[test]
    fn session_allow_is_broadening_error() {
        let src = r#"
[[rules]]
id = "session-grant"
effect = "allow"
capability = "fs.read"
"#;
        let source = PolicySource::session("session").expect("session");
        assert_eq!(
            parse(src, source),
            Err(PolicyParseError::Broadening {
                field: PolicyField::Effect,
            })
        );
    }

    #[test]
    fn unknown_privileged_match_fields_fail_closed() {
        for src in [
            r#"
bypass = true
[[rules]]
id = "a"
effect = "deny"
capability = "fs.read"
"#,
            r#"
[[rules]]
id = "a"
effect = "deny"
capability = "fs.read"
always = true
"#,
            r#"
[[rules]]
id = "a"
effect = "deny"
capability = "fs.read"
regex = ".*"
"#,
            r#"
[[rules]]
id = "a"
effect = "deny"
capability = "fs.read"
resource = { root = "repo", sudo = true }
"#,
            r#"
[[rules]]
id = "a"
effect = "deny"
capability = "fs.read"
conditions = { bypass = true }
"#,
            r#"
layer = "compiled"
[[rules]]
id = "a"
effect = "deny"
capability = "fs.read"
"#,
        ] {
            let err = parse(src, user()).expect_err("unknown field");
            assert!(
                matches!(err, PolicyParseError::UnknownPrivilegedField { .. }),
                "expected unknown privileged field, got {err:?}"
            );
        }
    }

    #[test]
    fn unknown_capability_and_wildcard_fail_closed() {
        let wildcard = parse(
            r#"
[[rules]]
id = "all"
effect = "deny"
capability = "*"
"#,
            user(),
        )
        .expect_err("wildcard");
        assert!(matches!(wildcard, PolicyParseError::Capability(_)));

        let unknown = parse(
            r#"
[[rules]]
id = "super"
effect = "deny"
capability = "superuser.all"
"#,
            user(),
        )
        .expect_err("unknown");
        assert!(matches!(unknown, PolicyParseError::Capability(_)));
    }

    #[test]
    fn invalid_syntax_and_schema_are_safe_errors() {
        assert_eq!(
            parse("[[[not toml", user()),
            Err(PolicyParseError::InvalidSyntax)
        );
        assert_eq!(
            parse("schema = 2\n", user()),
            Err(PolicyParseError::UnsupportedSchema { found: 2 })
        );
        let typed = parse(
            r#"
[[rules]]
id = 1
effect = "deny"
capability = "fs.read"
"#,
            user(),
        )
        .expect_err("typed");
        assert_eq!(
            typed,
            PolicyParseError::InvalidValue {
                field: PolicyField::Id,
            }
        );
    }

    #[test]
    fn duplicate_rule_ids_fail_closed() {
        let err = parse(
            r#"
[[rules]]
id = "same"
effect = "deny"
capability = "fs.read"

[[rules]]
id = "same"
effect = "ask"
capability = "fs.write"
"#,
            user(),
        )
        .expect_err("dup");
        assert_eq!(err, PolicyParseError::DuplicateRuleId);
    }

    #[test]
    fn oversized_document_and_cancellation_fail_closed() {
        let huge = "x".repeat(MAX_POLICY_DOCUMENT_BYTES + 1);
        assert_eq!(parse(&huge, user()), Err(PolicyParseError::SourceTooLarge));

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            PolicyDocument::parse_toml(allow_ask_deny_doc(), user(), &cancel),
            Err(PolicyParseError::Cancelled)
        );
    }

    #[test]
    fn errors_do_not_echo_secret_values() {
        let src = format!(
            r#"
[[rules]]
id = "leak"
effect = "{SECRET}"
capability = "secret.use"
resource = {{ secret_id = "{SECRET}", target = "env" }}
"#
        );
        let err = parse(&src, user()).expect_err("invalid effect");
        let text = err.to_string();
        assert_eq!(
            err,
            PolicyParseError::InvalidValue {
                field: PolicyField::Effect,
            }
        );
        assert!(!text.contains(SECRET));
        assert!(!format!("{err:?}").contains(SECRET));
    }

    #[test]
    fn empty_document_is_valid_with_source_trust() {
        let doc = parse("", workspace()).expect("empty");
        assert!(doc.rules().is_empty());
        assert_eq!(doc.source_trust(), PolicyLayer::TrustedProject);
    }

    #[test]
    fn org_and_compiled_may_allow() {
        let src = r#"
[[rules]]
id = "org-allow"
effect = "allow"
capability = "fs.read"
"#;
        let org = parse(src, PolicySource::organization("org.toml").unwrap()).expect("org");
        assert_eq!(org.rules()[0].effect(), PolicyEffect::Allow);
        let compiled = parse(src, PolicySource::compiled()).expect("compiled");
        assert_eq!(compiled.source_trust(), PolicyLayer::Compiled);
    }
}
