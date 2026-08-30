//! Six-mode permission lattice for the exec tool path (gaps.md §4 parity).
//!
//! Both reference CLIs converged on the same lattice: modes named exactly
//! `default | plan | acceptEdits | auto | dontAsk | bypassPermissions`, plus
//! `Tool(arg-glob)` allow/ask/deny rules where deny wins, remembered
//! per-project allow grants consulted before asking, and a typed reason on
//! every decision. This module is pure decision logic: no I/O, no prompts.
//! Headless exec cannot ask, so an `Ask` decision renders as a typed
//! model-visible denial at the driver — never a silent pass.

use std::collections::BTreeMap;
use std::fmt;

/// Hard ceiling on rules loaded for one decision context.
pub const MAX_RULES: usize = 128;
/// Hard ceiling on persisted grants consulted for one project.
pub const MAX_GRANTS: usize = 128;
/// Maximum UTF-8 bytes for one rule or grant pattern.
pub const MAX_PATTERN_BYTES: usize = 256;
/// Maximum UTF-8 bytes for the settings document.
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;
/// Maximum persisted projects in the grants file.
pub const MAX_GRANT_RECORDS: usize = 4096;

/// The six permission modes, named exactly as the reference CLIs name them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PermissionMode {
    Default,
    Plan,
    AcceptEdits,
    Auto,
    DontAsk,
    BypassPermissions,
}

pub const MODE_NAMES: [&str; 6] = [
    "default",
    "plan",
    "acceptEdits",
    "auto",
    "dontAsk",
    "bypassPermissions",
];

impl PermissionMode {
    /// Case-sensitive parse over the fixed name table (the mode names are
    /// wire-compatible identifiers, not free text).
    pub fn parse(raw: &str) -> Option<Self> {
        MODE_NAMES
            .iter()
            .position(|name| *name == raw)
            .map(|index| Self::ALL[index])
    }

    pub const fn as_str(self) -> &'static str {
        MODE_NAMES[self as usize]
    }

    const ALL: [Self; 6] = [
        Self::Default,
        Self::Plan,
        Self::AcceptEdits,
        Self::Auto,
        Self::DontAsk,
        Self::BypassPermissions,
    ];

    /// Permissiveness order for managed-policy ceiling gates (Modbit
    /// `CAP-001`: lower-trust layers may only restrict, never widen).
    /// `Plan` is deliberately the strictest of all six — it denies every
    /// write-classified call outright (`evaluate`'s `PlanModeDeny` arm),
    /// stricter than `Default`'s "ask" — not a position in `Self::ALL`'s
    /// declaration order, which exists only to match `MODE_NAMES`'s lookup
    /// table and carries no permissiveness meaning.
    pub const fn permissiveness_rank(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Default => 1,
            Self::AcceptEdits => 2,
            Self::Auto => 3,
            Self::DontAsk => 4,
            Self::BypassPermissions => 5,
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Effect of a `Tool(pattern)` rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RuleEffect {
    Allow,
    Ask,
    Deny,
}

/// How a call is classified for the mode table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ToolClass {
    /// Pure read: auto-allowed in every mode once rules/grants had their say.
    ReadOnly,
    /// Bounded file edit inside the workspace (`acceptEdits`/`auto` allow).
    FileEdit,
    /// Any other state-mutating call (process execution, …).
    Other,
}

/// A `Tool(arg-glob)` rule: `Name` matches every call of the tool,
/// `Name(pattern)` additionally matches the call subject (path for file
/// tools, joined argv for `shell_exec`) with `*`/`?` glob semantics.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolRule {
    pub effect: RuleEffect,
    pub pattern: ToolPattern,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolPattern {
    tool: String,
    arg_glob: Option<String>,
}

impl ToolPattern {
    /// Parse `Name` or `Name(glob)`. Unknown tools are accepted as patterns:
    /// a deny rule for a tool this build does not know still reads as intent.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > MAX_PATTERN_BYTES {
            return None;
        }
        let (tool, arg_glob) = match raw.split_once('(') {
            Some((tool, rest)) => {
                let arg = rest.strip_suffix(')')?;
                if tool.is_empty() || arg.is_empty() {
                    return None;
                }
                (tool, Some(arg.to_owned()))
            }
            None => (raw, None),
        };
        if !valid_rule_ident(tool) {
            return None;
        }
        Some(Self {
            tool: tool.to_owned(),
            arg_glob,
        })
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn arg_glob(&self) -> Option<&str> {
        self.arg_glob.as_deref()
    }

    fn matches(&self, tool: &str, subject: &str) -> bool {
        if self.tool != tool {
            return false;
        }
        match &self.arg_glob {
            None => true,
            Some(glob) => glob_match(glob, subject),
        }
    }
}

/// `true` when `value` matches `pattern` with `*` (any run) and `?` (one
/// char). Iterative single-pass matcher, no regex dependency.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let (mut p, mut v) = (0usize, 0usize);
    let (mut star, mut star_v) = (None::<usize>, 0usize);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            star_v = v;
            p += 1;
        } else if let Some(star_p) = star {
            p = star_p + 1;
            star_v += 1;
            v = star_v;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Is `subject` equal to `scope` or a path under it? Segment-aware: `scope`
/// = "src" must not match `subject` = "src-other/file.rs" (a naive string
/// prefix would).
fn path_within_scope(scope: &str, subject: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    if scope.is_empty() {
        return true;
    }
    subject == scope
        || subject
            .strip_prefix(scope)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn valid_rule_ident(tool: &str) -> bool {
    if tool.is_empty() || tool.len() > MAX_PATTERN_BYTES {
        return false;
    }
    let mut bytes = tool.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    tool.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Typed reason carried by every decision. `as_str` is a stable identifier;
/// `explanation` is the model-facing one-liner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum DecisionReason {
    DenyRule,
    AskRule,
    AllowRule,
    PersistedGrant,
    ReadOnlyAutoAllow,
    EditModeAllow,
    BypassAllow,
    ModeAsk,
    PlanModeDeny,
    DontAskDeny,
    UntrustedProject,
    /// Denied by a lattice-level `write_scope` ceiling (Modbit `CAP-008`/
    /// `AgentExecutionCapsule`'s narrow write scope) — a subagent confined
    /// to a subtree tried to mutate something outside it. Checked before
    /// every rule/grant/mode, since a scope ceiling is not something a
    /// lower-trust layer (a rule the child's own prompt could talk the
    /// model into proposing) may widen.
    WriteScopeViolation,
    /// Denied by an admin/managed-policy tool ban (Modbit `CAP-001`'s Policy
    /// Compiler: a hard, enterprise-level ceiling lower-trust layers — a
    /// project's rules, a user's grants, even `bypassPermissions` — may
    /// never widen past). Checked before every rule/grant/mode, same
    /// precedence as `WriteScopeViolation`.
    AdminToolDenied,
    /// Denied by an admin/managed-policy write-scope ceiling (Modbit
    /// `CAP-001`) — distinct from `WriteScopeViolation` (a `task_spawn`
    /// subagent's own narrower scope) so a denial reads as "the deployment
    /// confines every write here" rather than "this specific delegated
    /// task was scoped down." Checked before `write_scope`, every rule,
    /// grant, and mode.
    AdminWriteScopeViolation,
}

impl DecisionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DenyRule => "deny_rule",
            Self::AskRule => "ask_rule",
            Self::AllowRule => "allow_rule",
            Self::PersistedGrant => "persisted_grant",
            Self::ReadOnlyAutoAllow => "read_only_auto_allow",
            Self::EditModeAllow => "edit_mode_allow",
            Self::BypassAllow => "mode_allow",
            Self::ModeAsk => "mode_ask",
            Self::PlanModeDeny => "plan_mode_deny",
            Self::DontAskDeny => "dont_ask_deny",
            Self::UntrustedProject => "untrusted_project",
            Self::WriteScopeViolation => "write_scope_violation",
            Self::AdminToolDenied => "admin_tool_denied",
            Self::AdminWriteScopeViolation => "admin_write_scope_violation",
        }
    }

    /// Model-facing one-liner. Static text only.
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::DenyRule => "denied by an explicit deny rule",
            Self::AskRule => "an ask rule requires interactive approval",
            Self::AllowRule => "allowed by an explicit allow rule",
            Self::PersistedGrant => "allowed by a persisted per-project grant",
            Self::ReadOnlyAutoAllow => "allowed: read-only calls run without approval",
            Self::EditModeAllow => "allowed: the current mode auto-approves workspace edits",
            Self::BypassAllow => "allowed by bypassPermissions mode",
            Self::ModeAsk => "requires interactive approval; headless exec cannot ask",
            Self::PlanModeDeny => "plan mode is read-only; this call mutates state",
            Self::DontAskDeny => "dontAsk mode silently refuses calls that are not pre-approved",
            Self::UntrustedProject => "the project is not trusted; every tool call is refused",
            Self::WriteScopeViolation => "outside the write scope this subagent was confined to",
            Self::AdminToolDenied => "this tool is banned by managed policy; no setting can re-enable it",
            Self::AdminWriteScopeViolation => {
                "outside the write scope managed policy confines this deployment to"
            }
        }
    }
}

/// Outcome of one permission evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Decision {
    Allow(DecisionReason),
    Ask(DecisionReason),
    Deny(DecisionReason),
}

impl Decision {
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow(_))
    }

    pub const fn is_denied(self) -> bool {
        matches!(self, Self::Deny(_))
    }

    pub const fn reason(self) -> DecisionReason {
        match self {
            Self::Allow(reason) | Self::Ask(reason) | Self::Deny(reason) => reason,
        }
    }
}

/// The lattice: one mode, ordered rules, and persisted per-project grants.
/// Evaluation is total and deterministic — every call gets a typed decision.
#[derive(Clone, Debug)]
pub struct PermissionLattice {
    mode: PermissionMode,
    rules: Vec<ToolRule>,
    grants: Vec<ToolPattern>,
    /// Workspace-relative path prefix a write-classified call's subject
    /// must fall under (Modbit `CAP-008`: a narrow write scope for a
    /// subagent, checked before every rule/grant/mode — see
    /// `DecisionReason::WriteScopeViolation`). `None`: no additional
    /// restriction, today's unscoped behavior.
    write_scope: Option<String>,
    /// Tools an admin/managed policy has banned outright (Modbit `CAP-001`
    /// Policy Compiler: an enterprise-level ceiling), checked before every
    /// rule/grant/mode — see `DecisionReason::AdminToolDenied`. Empty:
    /// no additional restriction, today's unmanaged behavior.
    denied_tools: Vec<ToolPattern>,
    /// Workspace-relative path prefix an admin/managed policy confines
    /// every write-classified call to (Modbit `CAP-001`), independent of
    /// — and checked before — the per-`task_spawn` `write_scope` above.
    /// Deliberately a *separate* field rather than reusing `write_scope`:
    /// `with_write_scope` overwrites (by design, for `task_spawn`'s "each
    /// call sets this subagent's own scope" use), and a subagent's own
    /// scope argument overwriting an admin ceiling instead of narrowing
    /// within it would silently defeat the ceiling. `None`: no additional
    /// restriction, today's unmanaged behavior.
    admin_write_scope: Option<String>,
}

impl PermissionLattice {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            rules: Vec::new(),
            grants: Vec::new(),
            write_scope: None,
            denied_tools: Vec::new(),
            admin_write_scope: None,
        }
    }

    /// Add rules (deny/ask/allow stay distinct effects; matching order is
    /// deny > ask > allow). Excess rules beyond [`MAX_RULES`] are dropped —
    /// bounds are enforced at load, never panic.
    pub fn with_rules(mut self, rules: Vec<ToolRule>) -> Self {
        self.rules.truncate(MAX_RULES);
        self.rules.extend(rules.into_iter().take(MAX_RULES - self.rules.len()));
        self
    }

    /// Add persisted allow grants consulted before the mode would ask.
    pub fn with_grants(mut self, grants: Vec<ToolPattern>) -> Self {
        self.grants
            .extend(grants.into_iter().take(MAX_GRANTS - self.grants.len()));
        self
    }

    /// Confine every write-classified call to `scope` (a workspace-relative
    /// path prefix) or its descendants — read-classified calls are
    /// unaffected. Rules, grants, and mode can only make a write *harder* to
    /// get inside the scope; none of them can widen past it.
    pub fn with_write_scope(mut self, scope: impl Into<String>) -> Self {
        self.write_scope = Some(scope.into());
        self
    }

    /// Ban every tool matching `patterns` outright (Modbit `CAP-001`): no
    /// rule, grant, or mode checked afterward — including `bypassPermissions`
    /// — can re-allow one. Additive with any existing bans, never replaces
    /// them, so an admin ceiling set once at construction can't be narrowed
    /// away by a later call.
    pub fn with_denied_tools(mut self, patterns: impl IntoIterator<Item = ToolPattern>) -> Self {
        self.denied_tools.extend(patterns);
        self
    }

    /// Confine every write-classified call, deployment-wide, to `scope` or
    /// its descendants (Modbit `CAP-001`) — independent of `with_write_scope`,
    /// which a `task_spawn` call may still set its own (narrower-in-intent)
    /// scope through without affecting this one. See `admin_write_scope`'s
    /// own doc comment for why the two must not share a field.
    pub fn with_admin_write_scope(mut self, scope: impl Into<String>) -> Self {
        self.admin_write_scope = Some(scope.into());
        self
    }

    pub const fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn rules(&self) -> &[ToolRule] {
        &self.rules
    }

    pub fn write_scope(&self) -> Option<&str> {
        self.write_scope.as_deref()
    }

    pub fn denied_tools(&self) -> &[ToolPattern] {
        &self.denied_tools
    }

    pub fn admin_write_scope(&self) -> Option<&str> {
        self.admin_write_scope.as_deref()
    }

    /// Lattice for a `task_spawn` child. The child is the model's own choice
    /// of what to delegate and with what prompt — never the human's direct
    /// action — so it must not silently wield authority the human never
    /// reviewed. Rules and persisted grants carry over unchanged (a child can
    /// still *consume* what was already approved, so it isn't reduced to
    /// asking for everything from scratch), but `BypassPermissions` — the one
    /// mode that allows every call with no `Ask` step at all, per
    /// [`Self::evaluate`]'s mode table — is capped to `AcceptEdits`. Every
    /// other mode already denies (never silently allows) a non-file-edit call
    /// for a subagent, since subagents run headless-style with no interactive
    /// channel and `Ask` renders as a denial there; `BypassPermissions` is the
    /// only mode where that safety net doesn't already apply.
    pub fn for_subagent(&self) -> Self {
        Self {
            mode: match self.mode {
                PermissionMode::BypassPermissions => PermissionMode::AcceptEdits,
                other => other,
            },
            rules: self.rules.clone(),
            grants: self.grants.clone(),
            write_scope: self.write_scope.clone(),
            denied_tools: self.denied_tools.clone(),
            admin_write_scope: self.admin_write_scope.clone(),
        }
    }

    /// Evaluate one call. `tool` is the gateway tool name, `subject` the
    /// rule-matching context (workspace-relative path for file tools, joined
    /// argv for `shell_exec`).
    pub fn evaluate(&self, tool: &str, subject: &str, class: ToolClass) -> Decision {
        // -1. Admin/managed-policy tool ban, checked before absolutely
        // everything else, including the write-scope ceiling below — the
        // one restriction nothing downstream (a rule, a grant, any mode,
        // including bypassPermissions) may ever widen past.
        if self.denied_tools.iter().any(|pattern| pattern.matches(tool, subject)) {
            return Decision::Deny(DecisionReason::AdminToolDenied);
        }
        // -0.5. Admin/managed-policy write-scope ceiling — same precedence
        // as the tool ban above, and deliberately checked *before* the
        // per-`task_spawn` write_scope below: a subagent's own scope
        // argument must never be able to widen past a deployment-wide
        // confinement, only add a further restriction inside it.
        if class == ToolClass::FileEdit
            && let Some(scope) = &self.admin_write_scope
            && !path_within_scope(scope, subject)
        {
            return Decision::Deny(DecisionReason::AdminWriteScopeViolation);
        }
        // 0. Write-scope ceiling, checked before everything else — a rule,
        // grant, or mode may only make a write *harder* to get inside the
        // scope, never widen past it. Scoped to genuine file-edit calls
        // only: `shell_exec`'s `subject` is joined argv, not a workspace
        // path, so applying a path-prefix check to it would be meaningless.
        if class == ToolClass::FileEdit
            && let Some(scope) = &self.write_scope
            && !path_within_scope(scope, subject)
        {
            return Decision::Deny(DecisionReason::WriteScopeViolation);
        }
        // 1. Rules, by precedence not insertion order: deny wins, then ask,
        // then allow.
        for rule in &self.rules {
            if rule.effect == RuleEffect::Deny && rule.pattern.matches(tool, subject) {
                return Decision::Deny(DecisionReason::DenyRule);
            }
        }
        for rule in &self.rules {
            if rule.effect == RuleEffect::Ask && rule.pattern.matches(tool, subject) {
                return Decision::Ask(DecisionReason::AskRule);
            }
        }
        for rule in &self.rules {
            if rule.effect == RuleEffect::Allow && rule.pattern.matches(tool, subject) {
                return Decision::Allow(DecisionReason::AllowRule);
            }
        }
        // 2. Read-only calls run without approval in every mode.
        if class == ToolClass::ReadOnly {
            return Decision::Allow(DecisionReason::ReadOnlyAutoAllow);
        }
        // 3. Persisted per-project grants suppress the ask.
        if self
            .grants
            .iter()
            .any(|grant| grant.matches(tool, subject))
        {
            return Decision::Allow(DecisionReason::PersistedGrant);
        }
        // 4. Mode table.
        match self.mode {
            PermissionMode::Default => Decision::Ask(DecisionReason::ModeAsk),
            PermissionMode::Plan => Decision::Deny(DecisionReason::PlanModeDeny),
            PermissionMode::AcceptEdits | PermissionMode::Auto => {
                if class == ToolClass::FileEdit {
                    Decision::Allow(DecisionReason::EditModeAllow)
                } else {
                    Decision::Ask(DecisionReason::ModeAsk)
                }
            }
            PermissionMode::DontAsk => Decision::Deny(DecisionReason::DontAskDeny),
            PermissionMode::BypassPermissions => {
                Decision::Allow(DecisionReason::BypassAllow)
            }
        }
    }
}

/// A parsed project settings document (`permissions` rules + default mode).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectSettings {
    pub mode: Option<PermissionMode>,
    pub rules: Vec<ToolRule>,
}

/// Typed settings failure. Never echoes document content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsError {
    TooLarge,
    InvalidJson,
    InvalidRule,
    TooManyRules,
}

impl SettingsError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "settings document exceeds the size bound",
            Self::InvalidJson => "settings document is not valid JSON",
            Self::InvalidRule => "a permission rule is not a valid Tool(pattern)",
            Self::TooManyRules => "settings document exceeds the rule count bound",
        }
    }
}

/// Parse a settings document. Accepts both shapes the ecosystem writes:
/// `{"mode": "...", "permissions": {...}}` and the Claude-compatible
/// `{"permissions": {"defaultMode": "...", "allow": [...], "ask": [...],
/// "deny": [...]}}`. Unknown keys are ignored; bound violations fail typed.
pub fn parse_settings(text: &str) -> Result<ProjectSettings, SettingsError> {
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::TooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| SettingsError::InvalidJson)?;
    let object = value.as_object().ok_or(SettingsError::InvalidJson)?;
    let mut settings = ProjectSettings::default();
    if let Some(mode) = object.get("mode").and_then(serde_json::Value::as_str) {
        settings.mode = Some(
            PermissionMode::parse(mode).ok_or(SettingsError::InvalidRule)?,
        );
    }
    let Some(permissions) = object.get("permissions") else {
        return Ok(settings);
    };
    let permissions = permissions.as_object().ok_or(SettingsError::InvalidJson)?;
    if settings.mode.is_none()
        && let Some(mode) = permissions.get("defaultMode").and_then(serde_json::Value::as_str)
    {
        settings.mode = Some(
            PermissionMode::parse(mode).ok_or(SettingsError::InvalidRule)?,
        );
    }
    for (key, effect) in [
        ("deny", RuleEffect::Deny),
        ("ask", RuleEffect::Ask),
        ("allow", RuleEffect::Allow),
    ] {
        let Some(entries) = permissions.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for entry in entries {
            let raw = entry.as_str().ok_or(SettingsError::InvalidRule)?;
            let pattern = ToolPattern::parse(raw).ok_or(SettingsError::InvalidRule)?;
            settings.rules.push(ToolRule { effect, pattern });
            if settings.rules.len() > MAX_RULES {
                return Err(SettingsError::TooManyRules);
            }
        }
    }
    Ok(settings)
}

/// Persisted per-project allow grants, keyed by canonical project root.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PermissionGrants {
    records: BTreeMap<String, Vec<ToolPattern>>,
}

/// Typed grants-file failure. Corrupt input fails closed: callers treat it as
/// "no grants", never as "grant everything".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantsError {
    TooLarge,
    InvalidJson,
    InvalidGrant,
    TooManyRecords,
}

impl PermissionGrants {
    /// Allow patterns recorded for `canonical_root` (empty when absent).
    pub fn for_root(&self, canonical_root: &str) -> Vec<ToolPattern> {
        self.records
            .get(canonical_root)
            .cloned()
            .unwrap_or_default()
    }
}

/// Parse the persisted grants document:
/// `{"schema": 1, "projects": [{"root": "...", "allow": ["Tool(glob)"]}]}.
pub fn parse_grants(text: &str) -> Result<PermissionGrants, GrantsError> {
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(GrantsError::TooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| GrantsError::InvalidJson)?;
    let object = value.as_object().ok_or(GrantsError::InvalidJson)?;
    if object.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(GrantsError::InvalidJson);
    }
    let mut grants = PermissionGrants::default();
    let Some(projects) = object.get("projects").and_then(serde_json::Value::as_array) else {
        return Ok(grants);
    };
    if projects.len() > MAX_GRANT_RECORDS {
        return Err(GrantsError::TooManyRecords);
    }
    for project in projects {
        let project = project.as_object().ok_or(GrantsError::InvalidJson)?;
        let root = project
            .get("root")
            .and_then(serde_json::Value::as_str)
            .ok_or(GrantsError::InvalidJson)?;
        let mut allow = Vec::new();
        for entry in project
            .get("allow")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let raw = entry.as_str().ok_or(GrantsError::InvalidGrant)?;
            let pattern = ToolPattern::parse(raw).ok_or(GrantsError::InvalidGrant)?;
            allow.push(pattern);
            if allow.len() > MAX_GRANTS {
                break;
            }
        }
        grants.records.insert(root.to_owned(), allow);
    }
    Ok(grants)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_round_trip_exactly() {
        for name in MODE_NAMES {
            let mode = PermissionMode::parse(name).expect("mode");
            assert_eq!(mode.as_str(), name);
        }
        assert_eq!(PermissionMode::parse("Default"), None, "case-sensitive");
        assert_eq!(PermissionMode::parse("yolo"), None);
        assert_eq!(MODE_NAMES.len(), 6);
        // The exact Claude/Grok-compatible spelling of the accept-edits mode.
        assert_eq!(
            PermissionMode::parse("acceptEdits"),
            Some(PermissionMode::AcceptEdits)
        );
    }

    #[test]
    fn read_only_calls_auto_allow_in_default_mode() {
        let lattice = PermissionLattice::new(PermissionMode::Default);
        for tool in ["repo_read", "repo_search", "workspace_read"] {
            assert_eq!(
                lattice.evaluate(tool, "src/lib.rs", ToolClass::ReadOnly),
                Decision::Allow(DecisionReason::ReadOnlyAutoAllow),
                "{tool} must auto-allow"
            );
        }
        // A write in default mode asks (a headless denial at the driver).
        assert_eq!(
            lattice.evaluate("workspace_patch", "src/lib.rs", ToolClass::FileEdit),
            Decision::Ask(DecisionReason::ModeAsk)
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "git status", ToolClass::Other),
            Decision::Ask(DecisionReason::ModeAsk)
        );
    }

    #[test]
    fn write_calls_ask_or_deny_per_mode() {
        let subject = "src/lib.rs";
        let cases = [
            (PermissionMode::Default, Decision::Ask(DecisionReason::ModeAsk)),
            (
                PermissionMode::Plan,
                Decision::Deny(DecisionReason::PlanModeDeny),
            ),
            (
                PermissionMode::AcceptEdits,
                Decision::Allow(DecisionReason::EditModeAllow),
            ),
            (
                PermissionMode::Auto,
                Decision::Allow(DecisionReason::EditModeAllow),
            ),
            (
                PermissionMode::DontAsk,
                Decision::Deny(DecisionReason::DontAskDeny),
            ),
            (
                PermissionMode::BypassPermissions,
                Decision::Allow(DecisionReason::BypassAllow),
            ),
        ];
        for (mode, expected) in cases {
            let lattice = PermissionLattice::new(mode);
            assert_eq!(
                lattice.evaluate("workspace_patch", subject, ToolClass::FileEdit),
                expected,
                "{mode} file-edit decision"
            );
        }
        // Non-edit writes still ask under acceptEdits/auto.
        for mode in [PermissionMode::AcceptEdits, PermissionMode::Auto] {
            let lattice = PermissionLattice::new(mode);
            assert_eq!(
                lattice.evaluate("shell_exec", "rm -rf build", ToolClass::Other),
                Decision::Ask(DecisionReason::ModeAsk),
                "{mode} shell decision"
            );
        }
        // Plan mode allows reads.
        let plan = PermissionLattice::new(PermissionMode::Plan);
        assert!(plan
            .evaluate("repo_read", "src/lib.rs", ToolClass::ReadOnly)
            .is_allowed());
    }

    #[test]
    fn subagent_lattice_caps_bypass_but_leaves_every_other_mode_and_rules_alone() {
        let rule = ToolRule {
            effect: RuleEffect::Allow,
            pattern: ToolPattern::parse("shell_exec(*)").expect("pattern"),
        };
        let grant = ToolPattern::parse("workspace_patch(*)").expect("pattern");

        let bypass = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_rules(vec![rule.clone()])
            .with_grants(vec![grant.clone()]);
        let child = bypass.for_subagent();
        assert_eq!(child.mode(), PermissionMode::AcceptEdits);
        // A capable child still auto-allows file edits (it just lost the
        // "allow literally everything, never ask" ceiling), and the parent's
        // rules/grants carried over rather than starting from nothing.
        assert!(
            child
                .evaluate("workspace_write", "src/lib.rs", ToolClass::FileEdit)
                .is_allowed()
        );
        assert_eq!(
            child.evaluate("shell_exec", "cargo test", ToolClass::Other),
            Decision::Allow(DecisionReason::AllowRule),
            "carried-over allow rule still applies"
        );
        assert!(
            child
                .evaluate("workspace_patch", "src/lib.rs", ToolClass::FileEdit)
                .is_allowed(),
            "carried-over persisted grant still applies"
        );

        // Every other mode passes through unchanged: they were already safe
        // for a subagent (non-file-edit calls hit `Ask`, which a headless-style
        // subagent turn treats as a denial — see `evaluate`'s doc comment).
        for mode in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::DontAsk,
        ] {
            let lattice = PermissionLattice::new(mode);
            assert_eq!(lattice.for_subagent().mode(), mode, "{mode} passes through");
        }
    }

    #[test]
    fn write_scope_confines_file_edits_but_never_shell_exec() {
        // BypassPermissions would allow everything unconditionally — the
        // scope ceiling must still win over even the most permissive mode.
        let lattice =
            PermissionLattice::new(PermissionMode::BypassPermissions).with_write_scope("src/feature");

        assert_eq!(
            lattice.evaluate("workspace_write", "src/feature/mod.rs", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::BypassAllow),
            "inside the scope: unaffected"
        );
        assert_eq!(
            lattice.evaluate("workspace_write", "src/feature", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::BypassAllow),
            "the scope root itself counts as inside"
        );
        assert_eq!(
            lattice.evaluate("workspace_write", "src/other.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::WriteScopeViolation),
            "outside the scope: denied even under bypassPermissions"
        );
        assert_eq!(
            lattice.evaluate("workspace_write", "src/feature-other/x.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::WriteScopeViolation),
            "segment-aware: a sibling directory sharing the prefix string must not match"
        );
        // shell_exec's subject is joined argv, not a path — the scope must
        // never apply to it, or a legitimate command would be misdenied for
        // merely containing the scope string as a substring coincidence.
        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf src/other.rs", ToolClass::Other),
            Decision::Allow(DecisionReason::BypassAllow),
            "shell_exec is never subject to the write scope"
        );
    }

    #[test]
    fn write_scope_survives_for_subagent_narrowing() {
        let parent = PermissionLattice::new(PermissionMode::Default).with_write_scope("src");
        let child = parent.for_subagent();
        assert_eq!(child.write_scope(), Some("src"));
        assert_eq!(
            child.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::WriteScopeViolation)
        );
    }

    #[test]
    fn admin_denied_tools_win_over_bypass_permissions_and_allow_rules() {
        // BypassPermissions plus an explicit allow rule would allow this
        // call by every other mechanism the lattice has — the admin ban
        // must still win over both.
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_rules(vec![ToolRule {
                effect: RuleEffect::Allow,
                pattern: ToolPattern::parse("shell_exec").expect("pattern"),
            }])
            .with_denied_tools([ToolPattern::parse("shell_exec").expect("pattern")]);

        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf /", ToolClass::Other),
            Decision::Deny(DecisionReason::AdminToolDenied),
            "an admin tool ban wins over bypassPermissions and an explicit allow rule alike"
        );
        // An unrelated tool is unaffected.
        assert_eq!(
            lattice.evaluate("workspace_read", "src/lib.rs", ToolClass::ReadOnly),
            Decision::Allow(DecisionReason::ReadOnlyAutoAllow),
        );
    }

    #[test]
    fn admin_denied_tools_survive_for_subagent_narrowing() {
        let parent = PermissionLattice::new(PermissionMode::Default)
            .with_denied_tools([ToolPattern::parse("task_spawn").expect("pattern")]);
        let child = parent.for_subagent();
        assert_eq!(child.denied_tools().len(), 1);
        assert_eq!(
            child.evaluate("task_spawn", "explore", ToolClass::Other),
            Decision::Deny(DecisionReason::AdminToolDenied)
        );
    }

    #[test]
    fn admin_denied_tools_respect_their_own_arg_glob() {
        // A pattern with an arg glob only bans matching arguments, not the
        // tool outright — same semantics as an ordinary deny rule's glob.
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_denied_tools([ToolPattern::parse("shell_exec(rm *)").expect("pattern")]);

        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf /tmp/x", ToolClass::Other),
            Decision::Deny(DecisionReason::AdminToolDenied)
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "ls -la", ToolClass::Other),
            Decision::Allow(DecisionReason::BypassAllow),
            "a non-matching argv for the same tool is unaffected"
        );
    }

    #[test]
    fn admin_write_scope_wins_over_bypass_permissions_and_write_scope_never_overwrites_it() {
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_admin_write_scope("src");

        assert_eq!(
            lattice.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminWriteScopeViolation),
            "outside the admin ceiling: denied even under bypassPermissions"
        );
        assert_eq!(
            lattice.evaluate("workspace_write", "src/lib.rs", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::BypassAllow),
            "inside the admin ceiling: unaffected"
        );

        // The exact scenario this field exists to prevent: a `task_spawn`
        // call setting its own (unrelated, wider) write_scope must never be
        // able to widen past the admin ceiling — the two fields are
        // independent, so `with_write_scope` cannot overwrite
        // `admin_write_scope` the way it would if they shared one field.
        let subagent_widened = lattice.with_write_scope("docs");
        assert_eq!(
            subagent_widened.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminWriteScopeViolation),
            "a subagent's own write_scope must not escape the admin ceiling"
        );
        assert_eq!(
            subagent_widened.evaluate("workspace_write", "src/lib.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::WriteScopeViolation),
            "still confined by its own narrower write_scope too — both must hold"
        );
    }

    #[test]
    fn admin_write_scope_survives_for_subagent_narrowing() {
        let parent = PermissionLattice::new(PermissionMode::Default).with_admin_write_scope("src");
        let child = parent.for_subagent();
        assert_eq!(child.admin_write_scope(), Some("src"));
        assert_eq!(
            child.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminWriteScopeViolation)
        );
    }

    #[test]
    fn dont_ask_silently_denies_and_bypass_still_honors_deny_rules() {
        // dontAsk denies everything not pre-approved (rules/grants/read-only).
        let lattice = PermissionLattice::new(PermissionMode::DontAsk);
        assert_eq!(
            lattice.evaluate("shell_exec", "cargo test", ToolClass::Other),
            Decision::Deny(DecisionReason::DontAskDeny)
        );
        // ...but a pre-approved grant still allows.
        let lattice = lattice.with_grants(vec![
            ToolPattern::parse("shell_exec(cargo *)").expect("grant"),
        ]);
        assert_eq!(
            lattice.evaluate("shell_exec", "cargo test", ToolClass::Other),
            Decision::Allow(DecisionReason::PersistedGrant)
        );

        // bypassPermissions allows, except where a deny rule fires.
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions).with_rules(vec![
            ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("shell_exec(rm *)").expect("rule"),
            },
        ]);
        assert!(lattice
            .evaluate("workspace_patch", "any.rs", ToolClass::FileEdit)
            .is_allowed());
        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf /", ToolClass::Other),
            Decision::Deny(DecisionReason::DenyRule)
        );
    }

    #[test]
    fn deny_rule_beats_allow_rule_and_ask_rule() {
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions).with_rules(vec![
            ToolRule {
                effect: RuleEffect::Allow,
                pattern: ToolPattern::parse("shell_exec(git *)").expect("allow"),
            },
            ToolRule {
                effect: RuleEffect::Ask,
                pattern: ToolPattern::parse("shell_exec(git push*)").expect("ask"),
            },
            ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("shell_exec(git push --force*)").expect("deny"),
            },
        ]);
        assert_eq!(
            lattice.evaluate("shell_exec", "git push --force origin main", ToolClass::Other),
            Decision::Deny(DecisionReason::DenyRule),
            "deny must win over allow and ask"
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "git push origin main", ToolClass::Other),
            Decision::Ask(DecisionReason::AskRule),
            "ask must win over allow"
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "git status", ToolClass::Other),
            Decision::Allow(DecisionReason::AllowRule)
        );
        // A deny rule beats read-only auto-allow too.
        let lattice =
            lattice.with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("repo_read(.env*)").expect("deny"),
            }]);
        assert_eq!(
            lattice.evaluate("repo_read", ".env.local", ToolClass::ReadOnly),
            Decision::Deny(DecisionReason::DenyRule)
        );
    }

    #[test]
    fn persisted_grant_suppresses_the_mode_ask() {
        let lattice = PermissionLattice::new(PermissionMode::Default).with_grants(vec![
            ToolPattern::parse("shell_exec(git *)").expect("grant"),
            ToolPattern::parse("workspace_patch").expect("grant"),
        ]);
        assert_eq!(
            lattice.evaluate("shell_exec", "git diff", ToolClass::Other),
            Decision::Allow(DecisionReason::PersistedGrant)
        );
        assert_eq!(
            lattice.evaluate("workspace_patch", "src/a.rs", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::PersistedGrant)
        );
        // Un-granted calls still ask.
        assert_eq!(
            lattice.evaluate("shell_exec", "make all", ToolClass::Other),
            Decision::Ask(DecisionReason::ModeAsk)
        );
    }

    #[test]
    fn every_decision_carries_a_typed_reason() {
        let lattice = PermissionLattice::new(PermissionMode::Default).with_rules(vec![
            ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("shell_exec(sudo *)").expect("rule"),
            },
        ]);
        let decisions = [
            lattice.evaluate("shell_exec", "sudo rm x", ToolClass::Other),
            lattice.evaluate("shell_exec", "ls", ToolClass::Other),
            lattice.evaluate("repo_read", "a.rs", ToolClass::ReadOnly),
            lattice.evaluate("workspace_patch", "a.rs", ToolClass::FileEdit),
        ];
        for decision in decisions {
            let reason = decision.reason();
            assert!(!reason.as_str().is_empty());
            assert!(!reason.explanation().is_empty());
        }
    }

    #[test]
    fn glob_matcher_handles_star_question_and_literals() {
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("git *", "git push origin"));
        assert!(glob_match("git ?tatus", "git status"));
        assert!(!glob_match("git status", "git status --short"));
        assert!(!glob_match("cargo *", "cargo"));
        assert!(glob_match("cargo*", "cargo"));
        assert!(glob_match("a*b*c", "a-x-b-y-c"));
        assert!(!glob_match("a*b*c", "a-x-b-y-d"));
        assert!(glob_match("*.rs", "src/main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs.bak"));
        // Multi-byte subjects match on chars, never split a code point.
        assert!(glob_match("é*", "ééé"));
    }

    #[test]
    fn tool_pattern_parse_rejects_malformed_rules() {
        assert!(ToolPattern::parse("").is_none());
        assert!(ToolPattern::parse("(").is_none());
        assert!(ToolPattern::parse("shell_exec(").is_none());
        assert!(ToolPattern::parse("shell_exec()").is_none());
        assert!(ToolPattern::parse("(git *)").is_none());
        assert!(ToolPattern::parse("shell exec(git)").is_none());
        let pattern = ToolPattern::parse("shell_exec(git *)").expect("ok");
        assert_eq!(pattern.tool(), "shell_exec");
        assert_eq!(pattern.arg_glob(), Some("git *"));
        let bare = ToolPattern::parse("workspace_patch").expect("ok");
        assert_eq!(bare.arg_glob(), None);
    }

    #[test]
    fn settings_parse_reads_both_document_shapes() {
        let rapidlm = parse_settings(
            r#"{"mode": "acceptEdits", "permissions": {"deny": ["shell_exec(rm *)"]}}"#,
        )
        .expect("rapidlm shape");
        assert_eq!(rapidlm.mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(rapidlm.rules.len(), 1);
        assert_eq!(rapidlm.rules[0].effect, RuleEffect::Deny);

        let claude = parse_settings(
            r#"{"permissions": {"defaultMode": "plan", "allow": ["Read(*)", "repo_read"],
               "ask": ["Bash(git push*)"]}}"#,
        )
        .expect("claude shape");
        assert_eq!(claude.mode, Some(PermissionMode::Plan));
        assert_eq!(claude.rules.len(), 3);
        assert_eq!(claude.rules[0].effect, RuleEffect::Ask, "ask before allow");

        // Unknown tools parse as patterns (deny intent survives), unknown keys
        // are ignored, and an empty document parses to defaults.
        let compat = parse_settings(r#"{"permissions": {"deny": ["WebFetch(domain:x)"]}, "extra": 1}"#)
            .expect("compat");
        assert_eq!(compat.rules.len(), 1);
        let empty = parse_settings("{}").expect("empty");
        assert_eq!(empty, ProjectSettings::default());
    }

    #[test]
    fn settings_parse_fails_typed_on_corrupt_or_out_of_bounds_input() {
        assert_eq!(parse_settings("nope"), Err(SettingsError::InvalidJson));
        assert_eq!(parse_settings("[1]"), Err(SettingsError::InvalidJson));
        assert_eq!(
            parse_settings(r#"{"mode": "yolo"}"#),
            Err(SettingsError::InvalidRule)
        );
        assert_eq!(
            parse_settings(r#"{"permissions": {"allow": ["bad rule!!"]}}"#),
            Err(SettingsError::InvalidRule)
        );
        let many: Vec<String> = (0..=MAX_RULES)
            .map(|index| format!("tool{index}"))
            .collect();
        let document = format!(r#"{{"permissions": {{"allow": {many:?}}}}}"#);
        assert_eq!(
            parse_settings(&document),
            Err(SettingsError::TooManyRules)
        );
        let oversized = format!("\"{}\"", "x".repeat(MAX_SETTINGS_BYTES + 1));
        assert_eq!(parse_settings(&oversized), Err(SettingsError::TooLarge));
    }

    #[test]
    fn grants_parse_is_scoped_per_root_and_fails_closed() {
        let document = r#"{"schema": 1, "projects": [
            {"root": "/work/a", "allow": ["shell_exec(cargo *)"]},
            {"root": "/work/b", "allow": []}
        ]}"#;
        let grants = parse_grants(document).expect("grants");
        assert_eq!(grants.for_root("/work/a").len(), 1);
        assert!(grants.for_root("/work/b").is_empty());
        assert!(grants.for_root("/work/missing").is_empty());

        assert_eq!(
            parse_grants(r#"{"schema": 2, "projects": []}"#),
            Err(GrantsError::InvalidJson),
            "unsupported schema version must fail closed"
        );
        assert_eq!(
            parse_grants("{"),
            Err(GrantsError::InvalidJson),
            "corrupt file yields no grants"
        );
        assert_eq!(
            parse_grants(r#"{"schema": 1, "projects": [{"root": "/a", "allow": ["!!"]}]}"#),
            Err(GrantsError::InvalidGrant)
        );
    }

    #[test]
    fn rule_and_grant_bounds_are_enforced_at_load() {
        let lattice = PermissionLattice::new(PermissionMode::Default);
        let rules: Vec<ToolRule> = (0..MAX_RULES + 10)
            .map(|index| ToolRule {
                effect: RuleEffect::Allow,
                pattern: ToolPattern::parse(&format!("tool{index}")).expect("rule"),
            })
            .collect();
        let lattice = lattice.with_rules(rules);
        assert_eq!(lattice.rules().len(), MAX_RULES);
    }
}
