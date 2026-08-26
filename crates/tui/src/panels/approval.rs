//! Approval modal over a broker-normalized ask.
//!
//! [`ApprovalViewModel`] is a frontend projection. It never issues leases or
//! writes capability fields. Submit returns only `deny`, `approve_once`, or a
//! listed `approved_scope_id`. Expired or stale requests fail closed.
//! Untrusted reason/action/command text is sanitized. Env values are never
//! stored or shown. Threats: `T-004`, `T-012`, `T-013`, `T-017`.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use protocol::RedactionClass;

use crate::sanitize::sanitize_untrusted;
use crate::state::{ApprovalKey, ApprovalLifecycle, CancellationToken};

/// Maximum UTF-8 bytes retained for the exact normalized action text.
pub const MAX_ACTION_TEXT_BYTES: usize = 1024;

/// Maximum UTF-8 bytes retained for the untrusted caller reason.
pub const MAX_REASON_BYTES: usize = 512;

/// Maximum UTF-8 bytes retained for a policy origin label.
pub const MAX_POLICY_ORIGIN_BYTES: usize = 256;

/// Maximum UTF-8 bytes retained for one argv / env-name / rule-id token.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// Maximum listed scope rows.
pub const MAX_SCOPES: usize = 8;

/// Maximum ask-rule identifiers shown as policy source.
pub const MAX_RULE_IDS: usize = 16;

/// Maximum argv tokens retained in a command preview.
pub const MAX_ARGV: usize = 256;

/// Maximum environment names retained in a preview.
pub const MAX_ENV_NAMES: usize = 256;

/// Render width is clamped to this many columns.
pub const MAX_APPROVAL_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_APPROVAL_ROWS: u16 = 256;

const CANCEL_STRIDE: usize = 8;
const REDACTED: &str = "[REDACTED]";

/// Closed capability name copied from the broker normalized action.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CapabilityView {
    FsRead,
    FsWrite,
    ProcExec,
    NetConnect,
    GitWrite,
    SecretUse,
    BrowserNavigate,
    BrowserDownload,
    MobileControl,
    McpInvoke,
    PluginInvoke,
}

/// Coarse risk copied from the broker summary. Not a capability grant.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum RiskClassView {
    #[default]
    Low,
    Medium,
    High,
}

/// Policy layer that produced the ask. Lower layers cannot broaden.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PolicyLayerView {
    Compiled,
    Organization,
    User,
    TrustedProject,
    Session,
}

/// How a listed scope may be remembered. Does not broaden the resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ScopeKindView {
    OneShot,
    SessionExact,
}

/// Closed scope identifiers. Unknown strings cannot construct a broader grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalScopeId {
    Once,
    SessionExact,
}

/// Command invocation class shown in the preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CommandModeView {
    Argv,
    Shell,
}

/// Human choice returned by the modal. Never carries lease fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalModalChoice {
    Deny,
    ApproveOnce,
    ApprovedScopeId(ApprovalScopeId),
}

/// Operator intent. Free-form tokens are parsed against the listed scopes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalSubmitIntent<'a> {
    Deny,
    ApproveOnce,
    ApproveScope(ApprovalScopeId),
    RawToken(&'a str),
}

/// Typed modal failure. Display never echoes attacker-controlled input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalUiError {
    Cancelled,
    Expired,
    Stale,
    UnknownScope,
    FreeFormScopeRejected,
    BoundExceeded,
    InvalidField,
}

/// Broker-normalized action presented for review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalActionSpec {
    capability: CapabilityView,
    text: String,
    fingerprint: String,
}

/// Risk class plus untrusted caller reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRiskSpec {
    class: RiskClassView,
    reason: String,
}

/// Policy source that produced the ask. Origin is a label, not authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalPolicySpec {
    layer: PolicyLayerView,
    origin: String,
    rule_ids: Vec<String>,
}

/// One predefined safe scope. Constraints are never caller-authored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ListedScope {
    id: ApprovalScopeId,
    kind: ScopeKindView,
}

/// Closed listed scopes plus the default. Free-form leases are absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalScopeSpec {
    default: ApprovalScopeId,
    choices: Vec<ListedScope>,
}

/// Monotonic-second window copied from the broker request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ApprovalClock {
    created_at_secs: u64,
    expires_at_secs: u64,
}

/// Command/env preview input. Env *values* are discarded at construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPreviewInput {
    mode: CommandModeView,
    executable: String,
    argv: Vec<String>,
    cwd: String,
    env_names: Vec<String>,
}

/// Caller-supplied ask to project. Not a lease and not transferable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalPrompt {
    id: ApprovalKey,
    lifecycle: ApprovalLifecycle,
    action: ApprovalActionSpec,
    current_fingerprint: String,
    risk: ApprovalRiskSpec,
    policy: ApprovalPolicySpec,
    scopes: Vec<ListedScope>,
    default_scope: ApprovalScopeId,
    clock: ApprovalClock,
    command: Option<CommandPreviewInput>,
    redaction: RedactionClass,
}

/// Frontend-only projection. Never issues a lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalViewModel {
    id: ApprovalKey,
    lifecycle: ApprovalLifecycle,
    capability: CapabilityView,
    action_text: String,
    fingerprint: String,
    stale: bool,
    risk: RiskClassView,
    reason: String,
    policy_layer: PolicyLayerView,
    policy_origin: String,
    rule_ids: Vec<String>,
    scopes: Vec<ListedScope>,
    default_scope: ApprovalScopeId,
    selected: usize,
    clock: ApprovalClock,
    command: Option<CommandPreview>,
    redaction: RedactionClass,
}

/// Sanitized command/env preview. Values are always [`REDACTED`].
#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandPreview {
    mode: CommandModeView,
    executable: String,
    argv: Vec<String>,
    cwd: String,
    env_names: Vec<String>,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl CapabilityView {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::ProcExec => "proc.exec",
            Self::NetConnect => "net.connect",
            Self::GitWrite => "git.write",
            Self::SecretUse => "secret.use",
            Self::BrowserNavigate => "browser.navigate",
            Self::BrowserDownload => "browser.download",
            Self::MobileControl => "mobile.control",
            Self::McpInvoke => "mcp.invoke",
            Self::PluginInvoke => "plugin.invoke",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ApprovalUiError> {
        match raw {
            "fs.read" => Ok(Self::FsRead),
            "fs.write" => Ok(Self::FsWrite),
            "proc.exec" => Ok(Self::ProcExec),
            "net.connect" => Ok(Self::NetConnect),
            "git.write" => Ok(Self::GitWrite),
            "secret.use" => Ok(Self::SecretUse),
            "browser.navigate" => Ok(Self::BrowserNavigate),
            "browser.download" => Ok(Self::BrowserDownload),
            "mobile.control" => Ok(Self::MobileControl),
            "mcp.invoke" => Ok(Self::McpInvoke),
            "plugin.invoke" => Ok(Self::PluginInvoke),
            _ => Err(ApprovalUiError::InvalidField),
        }
    }
}

impl RiskClassView {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ApprovalUiError> {
        match raw {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(ApprovalUiError::InvalidField),
        }
    }
}

impl PolicyLayerView {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compiled => "compiled",
            Self::Organization => "organization",
            Self::User => "user",
            Self::TrustedProject => "trusted_project",
            Self::Session => "session",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ApprovalUiError> {
        match raw {
            "compiled" => Ok(Self::Compiled),
            "organization" => Ok(Self::Organization),
            "user" => Ok(Self::User),
            "trusted_project" => Ok(Self::TrustedProject),
            "session" => Ok(Self::Session),
            _ => Err(ApprovalUiError::InvalidField),
        }
    }
}

impl ScopeKindView {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneShot => "one-shot",
            Self::SessionExact => "session-exact",
        }
    }
}

impl ApprovalScopeId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::SessionExact => "session-exact",
        }
    }

    /// Parse a UI token. Only listed identifiers succeed.
    pub fn parse(raw: &str) -> Result<Self, ApprovalUiError> {
        match raw {
            "once" | "approve_once" => Ok(Self::Once),
            "session-exact" => Ok(Self::SessionExact),
            _ => Err(ApprovalUiError::FreeFormScopeRejected),
        }
    }
}

impl CommandModeView {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Argv => "argv",
            Self::Shell => "shell",
        }
    }
}

impl ApprovalModalChoice {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::ApproveOnce => "approve_once",
            Self::ApprovedScopeId(id) => id.as_str(),
        }
    }

    /// Lease ttl/uses/resource are never part of the modal result.
    pub const fn lease_fields(self) -> Option<ApprovalLeaseFields> {
        let _ = self;
        None
    }
}

/// Placeholder type proving the modal cannot return lease bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApprovalLeaseFields {}

impl ApprovalSubmitIntent<'_> {
    pub const fn as_static_str(self) -> Option<&'static str> {
        match self {
            Self::Deny => Some("deny"),
            Self::ApproveOnce => Some("approve_once"),
            Self::ApproveScope(id) => Some(id.as_str()),
            Self::RawToken(_) => None,
        }
    }
}

impl ApprovalUiError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "approval ui cancelled",
            Self::Expired => "approval expired",
            Self::Stale => "approval is stale",
            Self::UnknownScope => "approval scope is not listed",
            Self::FreeFormScopeRejected => "free-form approval scope is not accepted",
            Self::BoundExceeded => "approval projection resource bound exceeded",
            Self::InvalidField => "approval field is invalid",
        }
    }
}

impl ApprovalActionSpec {
    pub fn new(
        capability: CapabilityView,
        text: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Result<Self, ApprovalUiError> {
        let text = bound_text(text.into(), MAX_ACTION_TEXT_BYTES)?;
        let fingerprint = bound_fingerprint(fingerprint.into())?;
        Ok(Self {
            capability,
            text,
            fingerprint,
        })
    }

    pub const fn capability(&self) -> CapabilityView {
        self.capability
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl ApprovalRiskSpec {
    pub fn new(class: RiskClassView, reason: impl Into<String>) -> Result<Self, ApprovalUiError> {
        Ok(Self {
            class,
            reason: bound_text(reason.into(), MAX_REASON_BYTES)?,
        })
    }

    pub const fn class(&self) -> RiskClassView {
        self.class
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl ApprovalPolicySpec {
    pub fn new(
        layer: PolicyLayerView,
        origin: impl Into<String>,
        rule_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, ApprovalUiError> {
        let origin = bound_text(origin.into(), MAX_POLICY_ORIGIN_BYTES)?;
        let mut ids = Vec::new();
        for rule in rule_ids {
            if ids.len() >= MAX_RULE_IDS {
                return Err(ApprovalUiError::BoundExceeded);
            }
            ids.push(bound_token(rule.into())?);
        }
        Ok(Self {
            layer,
            origin,
            rule_ids: ids,
        })
    }

    pub const fn layer(&self) -> PolicyLayerView {
        self.layer
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn rule_ids(&self) -> &[String] {
        &self.rule_ids
    }
}

impl ApprovalScopeSpec {
    pub fn new(
        default: ApprovalScopeId,
        choices: impl Into<Vec<ListedScope>>,
    ) -> Result<Self, ApprovalUiError> {
        let choices = choices.into();
        if choices.is_empty() || choices.len() > MAX_SCOPES {
            return Err(ApprovalUiError::BoundExceeded);
        }
        if !choices.iter().any(|scope| scope.id == default) {
            return Err(ApprovalUiError::UnknownScope);
        }
        Ok(Self { default, choices })
    }

    pub fn default_safe() -> Self {
        Self {
            default: ApprovalScopeId::Once,
            choices: vec![ListedScope::once(), ListedScope::session_exact()],
        }
    }

    pub const fn default_scope(&self) -> ApprovalScopeId {
        self.default
    }

    pub fn choices(&self) -> &[ListedScope] {
        &self.choices
    }
}

impl ListedScope {
    pub const fn new(id: ApprovalScopeId, kind: ScopeKindView) -> Self {
        Self { id, kind }
    }

    pub const fn once() -> Self {
        Self::new(ApprovalScopeId::Once, ScopeKindView::OneShot)
    }

    pub const fn session_exact() -> Self {
        Self::new(ApprovalScopeId::SessionExact, ScopeKindView::SessionExact)
    }

    pub const fn id(self) -> ApprovalScopeId {
        self.id
    }

    pub const fn kind(self) -> ScopeKindView {
        self.kind
    }
}

impl ApprovalClock {
    pub fn new(created_at_secs: u64, expires_at_secs: u64) -> Result<Self, ApprovalUiError> {
        if expires_at_secs <= created_at_secs {
            return Err(ApprovalUiError::InvalidField);
        }
        Ok(Self {
            created_at_secs,
            expires_at_secs,
        })
    }

    pub const fn created_at_secs(self) -> u64 {
        self.created_at_secs
    }

    pub const fn expires_at_secs(self) -> u64 {
        self.expires_at_secs
    }

    pub const fn is_expired(self, now_secs: u64) -> bool {
        now_secs < self.created_at_secs || now_secs >= self.expires_at_secs
    }
}

impl CommandPreviewInput {
    pub fn argv(
        executable: impl Into<String>,
        argv: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<String>,
        env_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, ApprovalUiError> {
        Self::build(CommandModeView::Argv, executable, argv, cwd, env_names)
    }

    pub fn shell(
        executable: impl Into<String>,
        script: impl Into<String>,
        cwd: impl Into<String>,
        env_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, ApprovalUiError> {
        Self::build(CommandModeView::Shell, executable, [script], cwd, env_names)
    }

    fn build(
        mode: CommandModeView,
        executable: impl Into<String>,
        argv: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<String>,
        env_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, ApprovalUiError> {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        if argv.len() > MAX_ARGV {
            return Err(ApprovalUiError::BoundExceeded);
        }
        let env_names: Vec<String> = env_names.into_iter().map(Into::into).collect();
        if env_names.len() > MAX_ENV_NAMES {
            return Err(ApprovalUiError::BoundExceeded);
        }
        for token in argv.iter().chain(env_names.iter()) {
            bound_token(token.clone())?;
        }
        Ok(Self {
            mode,
            executable: bound_token(executable.into())?,
            argv,
            cwd: bound_token(cwd.into())?,
            env_names,
        })
    }

    pub const fn mode(&self) -> CommandModeView {
        self.mode
    }

    pub fn executable(&self) -> &str {
        &self.executable
    }

    pub fn args(&self) -> &[String] {
        &self.argv
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn env_names(&self) -> &[String] {
        &self.env_names
    }
}

impl ApprovalPrompt {
    pub fn new(
        id: ApprovalKey,
        lifecycle: ApprovalLifecycle,
        action: ApprovalActionSpec,
        risk: ApprovalRiskSpec,
        policy: ApprovalPolicySpec,
        scopes: ApprovalScopeSpec,
        clock: ApprovalClock,
    ) -> Result<Self, ApprovalUiError> {
        let fingerprint = action.fingerprint.clone();
        Ok(Self {
            id,
            lifecycle,
            action,
            current_fingerprint: fingerprint,
            risk,
            policy,
            scopes: scopes.choices,
            default_scope: scopes.default,
            clock,
            command: None,
            redaction: RedactionClass::Public,
        })
    }

    pub fn with_current_fingerprint(
        mut self,
        fingerprint: impl Into<String>,
    ) -> Result<Self, ApprovalUiError> {
        self.current_fingerprint = bound_fingerprint(fingerprint.into())?;
        Ok(self)
    }

    pub fn with_command(mut self, command: CommandPreviewInput) -> Self {
        self.command = Some(command);
        self
    }

    /// Env *values* are accepted only to prove they are discarded.
    pub fn with_env_assignments(
        mut self,
        pairs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Result<Self, ApprovalUiError> {
        let command = self.command.as_mut().ok_or(ApprovalUiError::InvalidField)?;
        for (name, value) in pairs {
            let _discarded_value = value.into();
            let name = bound_token(name.into())?;
            if command.env_names.len() >= MAX_ENV_NAMES {
                return Err(ApprovalUiError::BoundExceeded);
            }
            if !command.env_names.iter().any(|existing| existing == &name) {
                command.env_names.push(name);
            }
        }
        Ok(self)
    }

    pub fn with_redaction(mut self, redaction: RedactionClass) -> Self {
        self.redaction = redaction;
        self
    }

    pub fn id(&self) -> &ApprovalKey {
        &self.id
    }

    pub fn lifecycle(&self) -> ApprovalLifecycle {
        self.lifecycle
    }

    pub fn action(&self) -> &ApprovalActionSpec {
        &self.action
    }

    pub fn risk(&self) -> &ApprovalRiskSpec {
        &self.risk
    }

    pub fn policy(&self) -> &ApprovalPolicySpec {
        &self.policy
    }

    pub fn scopes(&self) -> &[ListedScope] {
        &self.scopes
    }

    pub fn default_scope(&self) -> ApprovalScopeId {
        self.default_scope
    }

    pub fn clock(&self) -> ApprovalClock {
        self.clock
    }
}

impl ApprovalViewModel {
    pub fn new(
        prompt: ApprovalPrompt,
        cancel: &CancellationToken,
    ) -> Result<Self, ApprovalUiError> {
        check_cancel(cancel)?;
        let stale = prompt.lifecycle != ApprovalLifecycle::Requested
            || prompt.current_fingerprint != prompt.action.fingerprint;
        let secret = prompt.redaction == RedactionClass::Secret;
        let action_text = if secret {
            REDACTED.to_owned()
        } else {
            sanitize_display(&prompt.action.text)
        };
        let reason = if secret {
            REDACTED.to_owned()
        } else {
            sanitize_display(&prompt.risk.reason)
        };
        let policy_origin = sanitize_display(&prompt.policy.origin);
        let mut rule_ids = Vec::with_capacity(prompt.policy.rule_ids.len());
        for (index, id) in prompt.policy.rule_ids.iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            rule_ids.push(sanitize_display(id));
        }
        let selected = prompt
            .scopes
            .iter()
            .position(|scope| scope.id == prompt.default_scope)
            .ok_or(ApprovalUiError::UnknownScope)?;
        let command = match prompt.command {
            Some(input) if secret => Some(redact_command(input, cancel, true)?),
            Some(input) => Some(redact_command(input, cancel, false)?),
            None => None,
        };
        Ok(Self {
            id: prompt.id,
            lifecycle: prompt.lifecycle,
            capability: prompt.action.capability,
            action_text,
            fingerprint: prompt.action.fingerprint,
            stale,
            risk: prompt.risk.class,
            reason,
            policy_layer: prompt.policy.layer,
            policy_origin,
            rule_ids,
            scopes: prompt.scopes,
            default_scope: prompt.default_scope,
            selected,
            clock: prompt.clock,
            command,
            redaction: prompt.redaction,
        })
    }

    pub fn id(&self) -> &ApprovalKey {
        &self.id
    }

    pub fn lifecycle(&self) -> ApprovalLifecycle {
        self.lifecycle
    }

    pub fn capability(&self) -> CapabilityView {
        self.capability
    }

    pub fn action_text(&self) -> &str {
        &self.action_text
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn risk(&self) -> RiskClassView {
        self.risk
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn policy_layer(&self) -> PolicyLayerView {
        self.policy_layer
    }

    pub fn policy_origin(&self) -> &str {
        &self.policy_origin
    }

    pub fn rule_ids(&self) -> &[String] {
        &self.rule_ids
    }

    pub fn scopes(&self) -> &[ListedScope] {
        &self.scopes
    }

    pub fn selected_scope(&self) -> ListedScope {
        self.scopes[self.selected]
    }

    pub fn default_scope(&self) -> ApprovalScopeId {
        self.default_scope
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn is_expired(&self, now_secs: u64) -> bool {
        self.lifecycle == ApprovalLifecycle::Expired || self.clock.is_expired(now_secs)
    }

    pub fn env_preview(&self) -> impl Iterator<Item = String> + '_ {
        self.command.iter().flat_map(|cmd| {
            cmd.env_names
                .iter()
                .map(|name| format!("{name}={REDACTED}"))
        })
    }

    /// This panel never issues, forges, or widens a capability lease.
    pub const fn issues_leases(&self) -> bool {
        false
    }

    pub fn select_scope(&self, index: usize) -> Result<Self, ApprovalUiError> {
        if index >= self.scopes.len() {
            return Err(ApprovalUiError::InvalidField);
        }
        let mut next = self.clone();
        next.selected = index;
        Ok(next)
    }

    pub fn select_next(&self) -> Self {
        let mut next = self.clone();
        if self.selected + 1 < self.scopes.len() {
            next.selected += 1;
        }
        next
    }

    pub fn select_prev(&self) -> Self {
        let mut next = self.clone();
        next.selected = self.selected.saturating_sub(1);
        next
    }

    /// Resolve a listed choice after expiry and staleness checks.
    ///
    /// Success is only [`ApprovalModalChoice`]. Arbitrary lease fields cannot
    /// be constructed from a token.
    pub fn submit(
        &self,
        intent: ApprovalSubmitIntent<'_>,
        now_secs: u64,
        cancel: &CancellationToken,
    ) -> Result<ApprovalModalChoice, ApprovalUiError> {
        check_cancel(cancel)?;
        if self.stale {
            return Err(ApprovalUiError::Stale);
        }
        if self.is_expired(now_secs) {
            return Err(ApprovalUiError::Expired);
        }
        let choice = match intent {
            ApprovalSubmitIntent::Deny => return Ok(ApprovalModalChoice::Deny),
            ApprovalSubmitIntent::ApproveOnce => ApprovalScopeId::Once,
            ApprovalSubmitIntent::ApproveScope(id) => id,
            ApprovalSubmitIntent::RawToken("deny") => return Ok(ApprovalModalChoice::Deny),
            ApprovalSubmitIntent::RawToken(raw) => parse_submit_token(raw)?,
        };
        if !self.scopes.iter().any(|scope| scope.id == choice) {
            return Err(ApprovalUiError::UnknownScope);
        }
        Ok(match choice {
            ApprovalScopeId::Once => ApprovalModalChoice::ApproveOnce,
            ApprovalScopeId::SessionExact => {
                ApprovalModalChoice::ApprovedScopeId(ApprovalScopeId::SessionExact)
            }
        })
    }

    pub fn render(&self, width: u16, height: u16) -> ApprovalFrame {
        let width = width.min(MAX_APPROVAL_COLS);
        let height = height.min(MAX_APPROVAL_ROWS);
        if width == 0 || height == 0 {
            return ApprovalFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let mut lines = Vec::new();
        lines.push(format!(
            "approval:{} {}",
            self.id.as_str(),
            lifecycle_label(self.lifecycle)
        ));
        if self.stale {
            lines.push("state:stale".to_owned());
        }
        lines.push(format!("capability:{}", self.capability.as_str()));
        lines.push(format!("action:{}", self.action_text));
        lines.push(format!("risk:{}", self.risk.as_str()));
        lines.push(format!("reason:{}", self.reason));
        lines.push(format!(
            "policy:{}:{}",
            self.policy_layer.as_str(),
            self.policy_origin
        ));
        if self.rule_ids.is_empty() {
            lines.push("rules:-".to_owned());
        } else {
            lines.push(format!("rules:{}", self.rule_ids.join(",")));
        }
        lines.push("scopes:".to_owned());
        for (index, scope) in self.scopes.iter().enumerate() {
            let marker = if index == self.selected { '>' } else { ' ' };
            lines.push(format!(
                "{marker} {} {}",
                scope.id.as_str(),
                scope.kind.as_str()
            ));
        }
        if let Some(command) = &self.command {
            let argv = command.argv.join(" ");
            lines.push(format!(
                "cmd: {} {} cwd={} {}",
                command.mode.as_str(),
                command.executable,
                command.cwd,
                argv
            ));
            if command.env_names.is_empty() {
                lines.push("env:-".to_owned());
            } else {
                let env = command
                    .env_names
                    .iter()
                    .map(|name| format!("{name}={REDACTED}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.push(format!("env: {env}"));
            }
        }
        if lines.len() > usize::from(height) {
            lines.truncate(usize::from(height));
        }
        ApprovalFrame {
            width,
            height,
            lines,
        }
    }
}

impl ApprovalFrame {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Stable dump used by golden tests. Trailing pad is omitted.
    pub fn golden(&self) -> String {
        self.lines.join("\n")
    }

    /// Exact-width rows written into the pane, padded/truncated to `height`.
    pub fn text(&self) -> String {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let mut rows = Vec::with_capacity(height);
        for i in 0..height {
            let src = self.lines.get(i).map(String::as_str).unwrap_or("");
            rows.push(fit_width(src, width));
        }
        rows.join("\n")
    }
}

impl Display for ApprovalUiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ApprovalUiError {}

impl Display for ApprovalModalChoice {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn parse_submit_token(raw: &str) -> Result<ApprovalScopeId, ApprovalUiError> {
    if raw == "deny" {
        return Err(ApprovalUiError::FreeFormScopeRejected);
    }
    ApprovalScopeId::parse(raw)
}

fn lifecycle_label(lifecycle: ApprovalLifecycle) -> &'static str {
    match lifecycle {
        ApprovalLifecycle::Requested => "requested",
        ApprovalLifecycle::Resolved => "resolved",
        ApprovalLifecycle::Expired => "expired",
    }
}

fn redact_command(
    input: CommandPreviewInput,
    cancel: &CancellationToken,
    force_redact: bool,
) -> Result<CommandPreview, ApprovalUiError> {
    check_cancel(cancel)?;
    let mut argv = Vec::with_capacity(input.argv.len());
    for (index, token) in input.argv.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        argv.push(redact_preview_token(token, force_redact));
    }
    let mut env_names = Vec::with_capacity(input.env_names.len());
    for (index, name) in input.env_names.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        env_names.push(sanitize_display(name));
    }
    Ok(CommandPreview {
        mode: input.mode,
        executable: redact_preview_token(&input.executable, force_redact),
        argv,
        cwd: sanitize_display(&input.cwd),
        env_names,
    })
}

fn redact_preview_token(token: &str, force_redact: bool) -> String {
    if force_redact || looks_secret_token(token) {
        return REDACTED.to_owned();
    }
    sanitize_display(token)
}

fn looks_secret_token(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if lower.contains("sk-")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("bearer ")
        || lower.contains("-----begin")
        || lower.contains("secret=")
        || lower.contains("token=")
        || lower.contains("password=")
        || lower.contains("authorization=")
    {
        return true;
    }
    if let Some((key, value)) = token.split_once('=') {
        let key = key.to_ascii_lowercase();
        if !value.is_empty()
            && (key.contains("secret")
                || key.contains("token")
                || key.contains("password")
                || key.contains("credential")
                || key.ends_with("key"))
        {
            return true;
        }
    }
    let body: String = token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    body.len() >= 32 && body.chars().all(|c| c.is_ascii_hexdigit())
}

fn sanitize_display(raw: &str) -> String {
    sanitize_untrusted(raw).into_owned()
}

fn bound_text(raw: String, max: usize) -> Result<String, ApprovalUiError> {
    if raw.len() > max || raw.contains('\0') {
        return Err(ApprovalUiError::BoundExceeded);
    }
    Ok(raw)
}

fn bound_token(raw: String) -> Result<String, ApprovalUiError> {
    if raw.is_empty() || raw.len() > MAX_TOKEN_BYTES || raw.contains('\0') {
        return Err(ApprovalUiError::BoundExceeded);
    }
    Ok(raw)
}

fn bound_fingerprint(raw: String) -> Result<String, ApprovalUiError> {
    if raw.is_empty() || raw.len() > 128 {
        return Err(ApprovalUiError::BoundExceeded);
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'-')
    {
        return Err(ApprovalUiError::InvalidField);
    }
    Ok(raw)
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cols = text.chars().count();
    if cols == width {
        return text.to_owned();
    }
    if cols < width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - cols));
        return out;
    }
    text.chars().take(width).collect()
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ApprovalUiError> {
    if cancel.is_cancelled() {
        Err(ApprovalUiError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPROVAL_ID: &str = "019c0000-0000-7000-8000-00000000001a";
    const FINGERPRINT: &str = "aabbccddeeff00112233445566778899";
    const SECRET: &str = "super-secret-password";
    const ENV_VALUE: &str = "super-secret-env-value";

    const GOLDEN_80: &str = "\
approval:019c0000-0000-7000-8000-00000000001a requested
capability:fs.read
action:fs.read repo:src/main.rs
risk:low
reason:read source
policy:trusted_project:.rapidlm/policy.toml
rules:repo-ask
scopes:
> once one-shot
  session-exact session-exact
cmd: argv /usr/bin/git cwd=/repo git status
env: HOME=[REDACTED] TOKEN=[REDACTED]";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn id() -> ApprovalKey {
        ApprovalKey::parse(APPROVAL_ID).expect("id")
    }

    fn clock() -> ApprovalClock {
        ApprovalClock::new(1_000, 1_060).expect("clock")
    }

    fn live_now() -> u64 {
        1_010
    }

    fn scopes() -> ApprovalScopeSpec {
        ApprovalScopeSpec::default_safe()
    }

    fn action() -> ApprovalActionSpec {
        ApprovalActionSpec::new(
            CapabilityView::FsRead,
            "fs.read repo:src/main.rs",
            FINGERPRINT,
        )
        .expect("action")
    }

    fn risk() -> ApprovalRiskSpec {
        ApprovalRiskSpec::new(RiskClassView::Low, "read source").expect("risk")
    }

    fn policy() -> ApprovalPolicySpec {
        ApprovalPolicySpec::new(
            PolicyLayerView::TrustedProject,
            ".rapidlm/policy.toml",
            ["repo-ask"],
        )
        .expect("policy")
    }

    fn command() -> CommandPreviewInput {
        CommandPreviewInput::argv(
            "/usr/bin/git",
            ["git", "status"],
            "/repo",
            ["HOME", "TOKEN"],
        )
        .expect("command")
    }

    fn fixture_prompt() -> ApprovalPrompt {
        ApprovalPrompt::new(
            id(),
            ApprovalLifecycle::Requested,
            action(),
            risk(),
            policy(),
            scopes(),
            clock(),
        )
        .expect("prompt")
        .with_command(command())
        .with_env_assignments([("TOKEN", ENV_VALUE)])
        .expect("env discarded")
    }

    fn fixture_model() -> ApprovalViewModel {
        ApprovalViewModel::new(fixture_prompt(), &cancel()).expect("model")
    }

    #[test]
    fn golden_80_120_200() {
        let model = fixture_model();
        assert_eq!(model.render(80, 16).golden(), GOLDEN_80);
        assert_eq!(model.render(120, 16).golden(), GOLDEN_80);
        assert_eq!(model.render(200, 16).golden(), GOLDEN_80);
        assert_eq!(model.render(80, 16).text().lines().count(), 16);
        assert!(
            model
                .render(80, 16)
                .text()
                .lines()
                .all(|line| line.chars().count() == 80)
        );
    }

    #[test]
    fn renders_normalized_action_risk_reason_policy_and_scopes() {
        let model = fixture_model();
        let golden = model.render(80, 16).golden();
        assert!(golden.contains("action:fs.read repo:src/main.rs"));
        assert!(golden.contains("risk:low"));
        assert!(golden.contains("reason:read source"));
        assert!(golden.contains("policy:trusted_project:.rapidlm/policy.toml"));
        assert!(golden.contains("rules:repo-ask"));
        assert!(golden.contains("> once one-shot"));
        assert!(golden.contains("  session-exact session-exact"));
    }

    #[test]
    fn submit_returns_closed_choice_tokens() {
        let model = fixture_model();
        let deny = model
            .submit(ApprovalSubmitIntent::Deny, live_now(), &cancel())
            .expect("deny");
        assert_eq!(deny, ApprovalModalChoice::Deny);
        assert_eq!(deny.as_str(), "deny");
        let once = model
            .submit(ApprovalSubmitIntent::ApproveOnce, live_now(), &cancel())
            .expect("once");
        assert_eq!(once, ApprovalModalChoice::ApproveOnce);
        assert_eq!(once.as_str(), "approve_once");
        let remembered = model
            .submit(
                ApprovalSubmitIntent::ApproveScope(ApprovalScopeId::SessionExact),
                live_now(),
                &cancel(),
            )
            .expect("scope");
        assert_eq!(
            remembered,
            ApprovalModalChoice::ApprovedScopeId(ApprovalScopeId::SessionExact)
        );
        assert_eq!(remembered.as_str(), "session-exact");
        assert_eq!(deny.lease_fields(), None);
        assert_eq!(once.lease_fields(), None);
        assert_eq!(remembered.lease_fields(), None);
        assert!(!model.issues_leases());
    }

    #[test]
    fn raw_tokens_map_only_to_listed_scopes() {
        let model = fixture_model();
        assert_eq!(
            model
                .submit(
                    ApprovalSubmitIntent::RawToken("deny"),
                    live_now(),
                    &cancel()
                )
                .expect("deny"),
            ApprovalModalChoice::Deny
        );
        assert_eq!(
            model
                .submit(
                    ApprovalSubmitIntent::RawToken("approve_once"),
                    live_now(),
                    &cancel()
                )
                .expect("approve_once"),
            ApprovalModalChoice::ApproveOnce
        );
        assert_eq!(
            model
                .submit(
                    ApprovalSubmitIntent::RawToken("session-exact"),
                    live_now(),
                    &cancel()
                )
                .expect("session"),
            ApprovalModalChoice::ApprovedScopeId(ApprovalScopeId::SessionExact)
        );
        assert_eq!(
            model
                .submit(
                    ApprovalSubmitIntent::RawToken(r#"{"ttl":3600,"uses":999,"root":"**"}"#),
                    live_now(),
                    &cancel(),
                )
                .expect_err("lease fields"),
            ApprovalUiError::FreeFormScopeRejected
        );
        assert_eq!(
            model
                .submit(
                    ApprovalSubmitIntent::RawToken("ttl=3600,uses=999"),
                    live_now(),
                    &cancel(),
                )
                .expect_err("free form"),
            ApprovalUiError::FreeFormScopeRejected
        );
        assert_eq!(
            ApprovalScopeId::parse("always-allow"),
            Err(ApprovalUiError::FreeFormScopeRejected)
        );
    }

    #[test]
    fn expired_approval_cannot_be_submitted_as_success() {
        let model = fixture_model();
        let err = model
            .submit(ApprovalSubmitIntent::ApproveOnce, 1_060, &cancel())
            .expect_err("expired");
        assert_eq!(err, ApprovalUiError::Expired);
        let rollback = model
            .submit(ApprovalSubmitIntent::ApproveOnce, 0, &cancel())
            .expect_err("clock rollback");
        assert_eq!(rollback, ApprovalUiError::Expired);
        let expired_prompt = ApprovalPrompt::new(
            id(),
            ApprovalLifecycle::Expired,
            action(),
            risk(),
            policy(),
            scopes(),
            clock(),
        )
        .expect("prompt");
        let expired = ApprovalViewModel::new(expired_prompt, &cancel()).expect("model");
        assert!(expired.is_stale());
        assert_eq!(
            expired
                .submit(ApprovalSubmitIntent::ApproveOnce, live_now(), &cancel())
                .expect_err("expired lifecycle"),
            ApprovalUiError::Stale
        );
    }

    #[test]
    fn stale_mutated_action_cannot_be_submitted_as_success() {
        let prompt = fixture_prompt()
            .with_current_fingerprint("00112233445566778899aabbccddeeff")
            .expect("fingerprint");
        let model = ApprovalViewModel::new(prompt, &cancel()).expect("model");
        assert!(model.is_stale());
        assert_eq!(
            model
                .submit(ApprovalSubmitIntent::ApproveOnce, live_now(), &cancel())
                .expect_err("stale"),
            ApprovalUiError::Stale
        );
        let resolved = ApprovalPrompt::new(
            id(),
            ApprovalLifecycle::Resolved,
            action(),
            risk(),
            policy(),
            scopes(),
            clock(),
        )
        .expect("prompt");
        let resolved = ApprovalViewModel::new(resolved, &cancel()).expect("model");
        assert_eq!(
            resolved
                .submit(ApprovalSubmitIntent::ApproveOnce, live_now(), &cancel())
                .expect_err("resolved"),
            ApprovalUiError::Stale
        );
    }

    #[test]
    fn secrets_are_redacted_from_command_and_env_previews() {
        let command = CommandPreviewInput::argv(
            "/usr/bin/curl",
            [
                "curl",
                "--header",
                "Authorization=Bearer hunter2",
                &format!("SECRET={SECRET}"),
            ],
            "/repo",
            ["HOME", "AWS_SECRET_ACCESS_KEY"],
        )
        .expect("command");
        let prompt = ApprovalPrompt::new(
            id(),
            ApprovalLifecycle::Requested,
            ApprovalActionSpec::new(
                CapabilityView::ProcExec,
                "proc.exec argv cwd=/repo curl",
                FINGERPRINT,
            )
            .expect("action"),
            risk(),
            policy(),
            scopes(),
            clock(),
        )
        .expect("prompt")
        .with_command(command)
        .with_env_assignments([("AWS_SECRET_ACCESS_KEY", SECRET), ("HOME", ENV_VALUE)])
        .expect("env discarded");
        let model = ApprovalViewModel::new(prompt, &cancel()).expect("model");
        let golden = model.render(120, 20).golden();
        assert!(!golden.contains(SECRET));
        assert!(!golden.contains(ENV_VALUE));
        assert!(!golden.contains("hunter2"));
        assert!(golden.contains("Authorization=[REDACTED]") || golden.contains("[REDACTED]"));
        assert!(golden.contains("AWS_SECRET_ACCESS_KEY=[REDACTED]"));
        assert!(golden.contains("HOME=[REDACTED]"));
        for line in model.env_preview() {
            assert!(line.ends_with(REDACTED));
            assert!(!line.contains(SECRET));
            assert!(!line.contains(ENV_VALUE));
        }
    }

    #[test]
    fn secret_redaction_class_withholds_untrusted_bodies() {
        let prompt = fixture_prompt().with_redaction(RedactionClass::Secret);
        let model = ApprovalViewModel::new(prompt, &cancel()).expect("model");
        let golden = model.render(80, 16).golden();
        assert!(golden.contains(&format!("action:{REDACTED}")));
        assert!(golden.contains(&format!("reason:{REDACTED}")));
        assert!(golden.contains("[REDACTED]"));
        assert!(!golden.contains("read source"));
        assert!(!golden.contains(ENV_VALUE));
    }

    #[test]
    fn untrusted_reason_cannot_inject_terminal_controls() {
        let reason = ApprovalRiskSpec::new(
            RiskClassView::High,
            "ok\u{1b}]52;c;c2VjcmV0\u{07}exfil\u{202e}bid",
        )
        .expect("risk");
        let prompt = ApprovalPrompt::new(
            id(),
            ApprovalLifecycle::Requested,
            action(),
            reason,
            policy(),
            scopes(),
            clock(),
        )
        .expect("prompt");
        let model = ApprovalViewModel::new(prompt, &cancel()).expect("model");
        let golden = model.render(80, 12).golden();
        assert!(!golden.contains('\u{001B}'));
        assert!(!golden.contains('\u{202e}'));
        assert!(golden.contains("reason:okexfilbid"));
    }

    #[test]
    fn unknown_listed_scope_fails_closed() {
        let prompt = ApprovalPrompt::new(
            id(),
            ApprovalLifecycle::Requested,
            action(),
            risk(),
            policy(),
            ApprovalScopeSpec::new(ApprovalScopeId::Once, vec![ListedScope::once()])
                .expect("once only"),
            clock(),
        )
        .expect("prompt");
        let model = ApprovalViewModel::new(prompt, &cancel()).expect("model");
        assert_eq!(
            model
                .submit(
                    ApprovalSubmitIntent::ApproveScope(ApprovalScopeId::SessionExact),
                    live_now(),
                    &cancel(),
                )
                .expect_err("not listed"),
            ApprovalUiError::UnknownScope
        );
    }

    #[test]
    fn cancellation_fails_closed() {
        let cancelled = cancel();
        cancelled.cancel();
        assert_eq!(
            ApprovalViewModel::new(fixture_prompt(), &cancelled).expect_err("cancel"),
            ApprovalUiError::Cancelled
        );
        let live = cancel();
        let model = ApprovalViewModel::new(fixture_prompt(), &live).expect("model");
        live.cancel();
        assert_eq!(
            model
                .submit(ApprovalSubmitIntent::ApproveOnce, live_now(), &live)
                .expect_err("cancel submit"),
            ApprovalUiError::Cancelled
        );
    }

    #[test]
    fn scope_cursor_moves_without_granting() {
        let model = fixture_model();
        assert_eq!(model.selected_scope().id(), ApprovalScopeId::Once);
        let next = model.select_next();
        assert_eq!(next.selected_scope().id(), ApprovalScopeId::SessionExact);
        assert!(!next.issues_leases());
        assert_eq!(
            next.select_scope(3).expect_err("oob"),
            ApprovalUiError::InvalidField
        );
    }
}
