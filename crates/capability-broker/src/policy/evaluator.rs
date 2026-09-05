//! Deterministic layered policy evaluator.
//!
//! `evaluate` is pure for the same stack, request, and cancellation state.
//! Higher-trust deny is final. Lower-trust layers may only narrow. Missing
//! grants fail closed as deny. Decision traces carry rule IDs and layers only.

use std::error::Error;
use std::fmt;

use protocol::SessionId;

use crate::capability::{
    Capability, CapabilityFamily, FilesystemRoot, Hostname, PathGlob, ResourceDescriptor,
};
use crate::normalize::command::{CancellationToken, CanonicalCommand};
use crate::normalize::fs::CanonicalFsAction;
use crate::normalize::network::{CanonicalNetworkTarget, IpClass};
use crate::policy::parser::{
    PolicyDocument, PolicyEffect, PolicyLayer, PolicyRule, ResourcePattern, RuleId, SubjectPattern,
};

/// Maximum policy documents accepted in one stack.
pub const MAX_POLICY_STACK_DOCUMENTS: usize = 32;

/// Maximum UTF-8 bytes in a principal name.
pub const MAX_PRINCIPAL_BYTES: usize = 128;

/// Maximum UTF-8 bytes in an action reason. Reasons are not matched.
pub const MAX_REASON_BYTES: usize = 512;

const CANCEL_STRIDE: usize = 16;

const LAYERS_HIGH_TO_LOW: [PolicyLayer; 5] = [
    PolicyLayer::Compiled,
    PolicyLayer::Organization,
    PolicyLayer::User,
    PolicyLayer::TrustedProject,
    PolicyLayer::Session,
];

/// Named principal used for subject matching. Not a secret handle.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PrincipalRef {
    name: String,
}

/// Normalized action bound into evaluation. Families must match the request.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum CanonicalAction {
    Command(CanonicalCommand),
    Filesystem(CanonicalFsAction),
    Network(CanonicalNetworkTarget),
    Resource {
        capability: Capability,
        resource: ResourceDescriptor,
    },
}

/// Layered documents. Construction sorts high-trust first and preserves
/// insertion order within a layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyStack {
    documents: Vec<PolicyDocument>,
}

/// Normalized action request. `reason` is never used as authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionRequest {
    principal: PrincipalRef,
    session_id: SessionId,
    capability: Capability,
    resource: ResourceDescriptor,
    normalized_action: CanonicalAction,
    reason: String,
}

/// Default lease envelope for an allow decision. Issuance is a later task.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct LeaseConstraints {
    max_uses: u32,
    max_ttl_secs: u32,
}

/// Ask payload: matched rule IDs only. Approval construction is a later task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AskExplanation {
    rule_ids: Vec<RuleId>,
}

/// Why the action was denied. Display never includes request values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DenyReason {
    DefaultDeny,
    MatchedRule,
    HigherTrustDeny,
    IncompatibleAction,
    NonInteractive,
}

/// Allow / ask / deny. Deny is most restrictive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow(LeaseConstraints),
    Ask(AskExplanation),
    Deny(DenyReason),
}

/// One applicable rule. Identity and layer only; no resource or secret values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchedRule {
    id: RuleId,
    layer: PolicyLayer,
    effect: PolicyEffect,
    considered: RuleConsideration,
}

/// How a matched rule contributed. Ignored grants cannot broaden.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RuleConsideration {
    Applied,
    IgnoredCannotGrant,
    IgnoredCannotBroaden,
}

/// Coarse risk for explain/audit. Derived from capability family, not values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum RiskClass {
    Low,
    Medium,
    High,
}

/// Decision plus the applicable-rule trace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionWithTrace {
    decision: Decision,
    matched_rules: Vec<MatchedRule>,
    risk: RiskClass,
    explanation: String,
}

/// Typed evaluate failure. Display never echoes attacker-controlled input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PolicyEvalError {
    Cancelled,
    StackTooLarge,
    InvalidPrincipal,
    ReasonTooLarge,
}

impl PrincipalRef {
    pub fn parse(name: &str) -> Result<Self, PolicyEvalError> {
        if name.is_empty() || name.len() > MAX_PRINCIPAL_BYTES {
            return Err(PolicyEvalError::InvalidPrincipal);
        }
        if name.contains('\0') || name.chars().any(char::is_control) {
            return Err(PolicyEvalError::InvalidPrincipal);
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/'))
        {
            return Err(PolicyEvalError::InvalidPrincipal);
        }
        Ok(Self {
            name: name.to_owned(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }
}

impl PolicyStack {
    pub fn empty() -> Self {
        Self {
            documents: Vec::new(),
        }
    }

    /// Accept documents in any order. High-trust layers are evaluated first.
    pub fn new(
        documents: impl IntoIterator<Item = PolicyDocument>,
    ) -> Result<Self, PolicyEvalError> {
        let documents: Vec<PolicyDocument> = documents.into_iter().collect();
        if documents.len() > MAX_POLICY_STACK_DOCUMENTS {
            return Err(PolicyEvalError::StackTooLarge);
        }
        let mut indexed: Vec<(usize, PolicyDocument)> = documents.into_iter().enumerate().collect();
        indexed.sort_by_key(|(index, doc)| (layer_rank(doc.source_trust()), *index));
        Ok(Self {
            documents: indexed.into_iter().map(|(_, doc)| doc).collect(),
        })
    }

    pub fn documents(&self) -> &[PolicyDocument] {
        &self.documents
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }
}

impl ActionRequest {
    pub fn new(
        principal: PrincipalRef,
        session_id: SessionId,
        capability: Capability,
        resource: ResourceDescriptor,
        normalized_action: CanonicalAction,
        reason: impl Into<String>,
    ) -> Result<Self, PolicyEvalError> {
        let reason = reason.into();
        if reason.len() > MAX_REASON_BYTES {
            return Err(PolicyEvalError::ReasonTooLarge);
        }
        if reason.contains('\0') {
            return Err(PolicyEvalError::ReasonTooLarge);
        }
        Ok(Self {
            principal,
            session_id,
            capability,
            resource,
            normalized_action,
            reason,
        })
    }

    pub fn principal(&self) -> &PrincipalRef {
        &self.principal
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }

    pub fn normalized_action(&self) -> &CanonicalAction {
        &self.normalized_action
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl LeaseConstraints {
    pub const DEFAULT_MAX_USES: u32 = 1;
    pub const DEFAULT_MAX_TTL_SECS: u32 = 60;

    pub const fn standard() -> Self {
        Self {
            max_uses: Self::DEFAULT_MAX_USES,
            max_ttl_secs: Self::DEFAULT_MAX_TTL_SECS,
        }
    }

    pub const fn max_uses(self) -> u32 {
        self.max_uses
    }

    pub const fn max_ttl_secs(self) -> u32 {
        self.max_ttl_secs
    }
}

impl AskExplanation {
    pub fn rule_ids(&self) -> &[RuleId] {
        &self.rule_ids
    }
}

impl DenyReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultDeny => "default-deny",
            Self::MatchedRule => "matched-rule",
            Self::HigherTrustDeny => "higher-trust-deny",
            Self::IncompatibleAction => "incompatible-action",
            Self::NonInteractive => "noninteractive-ask-turned-deny",
        }
    }
}

/// Noninteractive (`dont-ask`) transform: an `Ask` decision fails closed as a
/// deny rather than prompting. `Allow` and `Deny` are unchanged.
pub fn dont_ask(decision: Decision) -> Decision {
    match decision {
        Decision::Ask(_) => Decision::Deny(DenyReason::NonInteractive),
        other => other,
    }
}

impl Decision {
    pub fn effect(&self) -> PolicyEffect {
        match self {
            Self::Allow(_) => PolicyEffect::Allow,
            Self::Ask(_) => PolicyEffect::Ask,
            Self::Deny(_) => PolicyEffect::Deny,
        }
    }
}

impl MatchedRule {
    pub fn id(&self) -> &RuleId {
        &self.id
    }

    pub fn layer(&self) -> PolicyLayer {
        self.layer
    }

    pub fn effect(&self) -> PolicyEffect {
        self.effect
    }

    pub fn considered(&self) -> RuleConsideration {
        self.considered
    }
}

impl RiskClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl DecisionWithTrace {
    pub fn decision(&self) -> &Decision {
        &self.decision
    }

    pub fn effect(&self) -> PolicyEffect {
        self.decision.effect()
    }

    pub fn matched_rules(&self) -> &[MatchedRule] {
        &self.matched_rules
    }

    pub fn matched_rule_ids(&self) -> impl Iterator<Item = &str> {
        self.matched_rules.iter().map(|rule| rule.id.as_str())
    }

    pub fn risk(&self) -> RiskClass {
        self.risk
    }

    /// Human-readable explain string. Rule IDs and layers only.
    pub fn explanation(&self) -> &str {
        &self.explanation
    }
}

impl PolicyEvalError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "policy evaluation cancelled",
            Self::StackTooLarge => "policy stack exceeds document bound",
            Self::InvalidPrincipal => "invalid policy principal",
            Self::ReasonTooLarge => "policy action reason exceeds bound",
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for RiskClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for PolicyEvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for PolicyEvalError {}

/// Evaluate a normalized action against layered policy.
///
/// Same stack, request, and cancellation state always produce the same result.
pub fn evaluate(
    stack: &PolicyStack,
    request: &ActionRequest,
    cancel: &CancellationToken,
) -> Result<DecisionWithTrace, PolicyEvalError> {
    if cancel.is_cancelled() {
        return Err(PolicyEvalError::Cancelled);
    }

    let risk = risk_class(request.capability, &request.resource);
    if !request_is_compatible(request) {
        return Ok(DecisionWithTrace {
            decision: Decision::Deny(DenyReason::IncompatibleAction),
            matched_rules: Vec::new(),
            risk,
            explanation: "deny: incompatible-action".to_owned(),
        });
    }
    let mut matched_rules = Vec::new();
    let mut ceiling: Option<PolicyEffect> = None;
    let mut deciding_layer: Option<PolicyLayer> = None;
    let mut scanned = 0usize;

    for layer in LAYERS_HIGH_TO_LOW {
        let mut layer_matches = Vec::new();
        for document in stack
            .documents
            .iter()
            .filter(|doc| doc.source_trust() == layer)
        {
            for rule in document.rules() {
                if scanned.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
                    return Err(PolicyEvalError::Cancelled);
                }
                scanned = scanned.saturating_add(1);
                if rule_matches(rule, request) {
                    layer_matches.push(rule);
                }
            }
        }

        let raw_opinion = layer_opinion(&layer_matches);
        let (applied, consideration) = consider_layer(layer, ceiling, raw_opinion);
        for rule in &layer_matches {
            matched_rules.push(MatchedRule {
                id: rule.id().clone(),
                layer,
                effect: rule.effect(),
                considered: rule_consideration(rule.effect(), applied, consideration, ceiling),
            });
        }

        if let Some(next) = applied {
            if ceiling.is_none() || restrictiveness(next) > restrictiveness_opt(ceiling) {
                deciding_layer = Some(layer);
            }
            ceiling = Some(match ceiling {
                None => next,
                Some(cur) => most_restrictive(cur, next),
            });
        }
    }

    let effect = if network_metadata_blocked(&request.normalized_action) {
        PolicyEffect::Deny
    } else {
        ceiling.unwrap_or(PolicyEffect::Deny)
    };
    let decision = match effect {
        PolicyEffect::Allow => Decision::Allow(LeaseConstraints::standard()),
        PolicyEffect::Ask => Decision::Ask(AskExplanation {
            rule_ids: matched_rules
                .iter()
                .filter(|rule| {
                    rule.effect == PolicyEffect::Ask
                        && rule.considered == RuleConsideration::Applied
                })
                .map(|rule| rule.id.clone())
                .collect(),
        }),
        PolicyEffect::Deny => Decision::Deny(
            if network_metadata_blocked(&request.normalized_action)
                && !matches!(ceiling, Some(PolicyEffect::Deny))
            {
                DenyReason::DefaultDeny
            } else {
                deny_reason(ceiling, deciding_layer)
            },
        ),
    };
    let explanation = build_explanation(&decision, &matched_rules);
    Ok(DecisionWithTrace {
        decision,
        matched_rules,
        risk,
        explanation,
    })
}

fn request_is_compatible(request: &ActionRequest) -> bool {
    if request
        .capability
        .compatible_with(&request.resource)
        .is_err()
    {
        return false;
    }
    match &request.normalized_action {
        CanonicalAction::Command(_) => request.capability.family() == CapabilityFamily::Proc,
        CanonicalAction::Filesystem(action) => {
            if request.capability.family() != CapabilityFamily::Fs {
                return false;
            }
            if action.root()
                != match &request.resource {
                    ResourceDescriptor::Filesystem(scope) => scope.root(),
                    _ => return false,
                }
            {
                return false;
            }
            fs_capability_matches_op(request.capability, action)
        }
        CanonicalAction::Network(_) => request.capability.family() == CapabilityFamily::Net,
        CanonicalAction::Resource {
            capability,
            resource,
        } => {
            *capability == request.capability
                && resource == &request.resource
                && capability.compatible_with(resource).is_ok()
        }
    }
}

fn fs_capability_matches_op(capability: Capability, action: &CanonicalFsAction) -> bool {
    use crate::normalize::fs::FsOpKind;
    matches!(
        (capability, action.operation()),
        (Capability::FsRead, FsOpKind::Read)
            | (
                Capability::FsWrite,
                FsOpKind::Write | FsOpKind::Create | FsOpKind::Delete | FsOpKind::Rename,
            )
    )
}

fn rule_matches(rule: &PolicyRule, request: &ActionRequest) -> bool {
    if !subject_matches(rule.subjects(), &request.principal) {
        return false;
    }
    if rule.capability_pattern() != request.capability {
        return false;
    }
    resource_matches(
        rule.resource_pattern(),
        rule.effect(),
        request.capability,
        &request.resource,
        &request.normalized_action,
    )
}

fn subject_matches(subjects: &[SubjectPattern], principal: &PrincipalRef) -> bool {
    subjects.iter().any(|subject| match subject {
        SubjectPattern::Any => true,
        SubjectPattern::Named(name) => name == principal.as_str(),
    })
}

fn resource_matches(
    pattern: &ResourcePattern,
    effect: PolicyEffect,
    capability: Capability,
    resource: &ResourceDescriptor,
    action: &CanonicalAction,
) -> bool {
    if resource.family() != capability.family() {
        return false;
    }
    match pattern {
        ResourcePattern::Any => !matches!(
            action,
            CanonicalAction::Network(net)
                if net.ip_classes().contains(&IpClass::MetadataLike)
                    && effect != PolicyEffect::Deny
        ),
        ResourcePattern::Filesystem { root, glob } => match resource {
            ResourceDescriptor::Filesystem(scope) => {
                absent_or_eq(root.as_ref(), &scope.root())
                    && match action {
                        CanonicalAction::Filesystem(fs) => {
                            fs_action_matches(root, glob.as_ref(), scope.glob(), fs, effect)
                        }
                        CanonicalAction::Resource { .. } => {
                            glob_field_matches(glob.as_ref(), scope.glob().as_str())
                        }
                        _ => false,
                    }
            }
            _ => false,
        },
        ResourcePattern::Process { command_family } => match resource {
            ResourceDescriptor::Process(scope) => absent_or_str(
                command_family.as_ref().map(|value| value.as_str()),
                scope.command_family().as_str(),
            ),
            _ => false,
        },
        ResourcePattern::Network { scheme, host, port } => match resource {
            ResourceDescriptor::Network(scope) => {
                absent_or_eq(scheme.as_ref(), &scope.scheme())
                    && absent_or_str(
                        host.as_ref().map(|value| value.as_str()),
                        scope.host().as_str(),
                    )
                    && absent_or_eq(port.as_ref(), &scope.port())
                    && match action {
                        CanonicalAction::Network(net) => net_action_matches(
                            scheme.as_ref(),
                            host.as_ref(),
                            port.as_ref(),
                            net,
                            effect,
                        ),
                        CanonicalAction::Resource { .. } => true,
                        _ => false,
                    }
            }
            _ => false,
        },
        ResourcePattern::Git { ref_scope } => match resource {
            ResourceDescriptor::Git(scope) => ref_scope
                .as_ref()
                .is_none_or(|expected| glob_matches(expected.as_str(), scope.ref_scope().as_str())),
            _ => false,
        },
        ResourcePattern::Secret { secret_id, target } => match resource {
            ResourceDescriptor::Secret(scope) => {
                absent_or_str(
                    secret_id.as_ref().map(|value| value.as_str()),
                    scope.secret_id().as_str(),
                ) && absent_or_str(
                    target.as_ref().map(|value| value.as_str()),
                    scope.target().as_str(),
                )
            }
            _ => false,
        },
        ResourcePattern::Browser { origin, path } => match resource {
            ResourceDescriptor::Browser(scope) => {
                absent_or_eq(origin.as_ref(), scope.origin())
                    && match (path.as_ref(), scope.path()) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(expected), Some(got)) => {
                            glob_matches(expected.as_str(), got.as_str())
                        }
                    }
            }
            _ => false,
        },
        ResourcePattern::Mobile { device_id } => match resource {
            ResourceDescriptor::Mobile(scope) => absent_or_str(
                device_id.as_ref().map(|value| value.as_str()),
                scope.device_id().as_str(),
            ),
            _ => false,
        },
        ResourcePattern::Mcp { server, tool } => match resource {
            ResourceDescriptor::Mcp(scope) => {
                absent_or_str(server.as_deref(), scope.server())
                    && absent_or_str(tool.as_deref(), scope.tool())
            }
            _ => false,
        },
        ResourcePattern::Plugin {
            plugin,
            capability: plugin_capability,
        } => match resource {
            ResourceDescriptor::Plugin(scope) => {
                absent_or_str(plugin.as_deref(), scope.plugin())
                    && absent_or_str(plugin_capability.as_deref(), scope.capability())
            }
            _ => false,
        },
    }
}

fn fs_action_matches(
    root: &Option<FilesystemRoot>,
    glob: Option<&PathGlob>,
    resource_glob: &PathGlob,
    action: &CanonicalFsAction,
    effect: PolicyEffect,
) -> bool {
    if !absent_or_eq(root.as_ref(), &action.root()) {
        return false;
    }
    let resource_hit = glob_field_matches(glob, resource_glob.as_str());
    let source_hit = glob_field_matches(glob, action.path().as_str());
    let dest_hit = action
        .dest()
        .is_some_and(|dest| glob_field_matches(glob, dest.as_str()));
    match effect {
        // Grants stay inside the allowed root/glob on every endpoint.
        PolicyEffect::Allow => resource_hit && source_hit && (action.dest().is_none() || dest_hit),
        // Restrictions apply if the source *or* dest is in the denied/asked path.
        PolicyEffect::Deny | PolicyEffect::Ask => resource_hit || source_hit || dest_hit,
    }
}

fn network_metadata_blocked(action: &CanonicalAction) -> bool {
    match action {
        CanonicalAction::Network(net) => net.ip_classes().contains(&IpClass::MetadataLike),
        _ => false,
    }
}

fn net_action_matches(
    scheme: Option<&crate::capability::NetworkScheme>,
    host: Option<&Hostname>,
    port: Option<&u16>,
    net: &CanonicalNetworkTarget,
    effect: PolicyEffect,
) -> bool {
    if !absent_or_eq(scheme, &net.scheme()) {
        return false;
    }
    if !host.is_none_or(|expected| net_host_matches(expected.as_str(), net)) {
        return false;
    }
    if !absent_or_eq(port, &net.port()) {
        return false;
    }
    if net.ip_classes().contains(&IpClass::MetadataLike) {
        return effect == PolicyEffect::Deny;
    }
    if effect == PolicyEffect::Allow && hostname_only_allow_uncovered(host, net) {
        return false;
    }
    true
}

/// Hostname-only allows do not cover unused classified IPs (DNS rebind).
fn hostname_only_allow_uncovered(host: Option<&Hostname>, net: &CanonicalNetworkTarget) -> bool {
    if !is_hostname_only_pattern(host) {
        return false;
    }
    net.ip_classes()
        .iter()
        .any(|class| *class != IpClass::Public)
        || unused_classified_ips(host, net)
}

fn is_hostname_only_pattern(host: Option<&Hostname>) -> bool {
    match host {
        None => true,
        Some(name) => name.as_str().parse::<std::net::IpAddr>().is_err(),
    }
}

fn unused_classified_ips(host: Option<&Hostname>, net: &CanonicalNetworkTarget) -> bool {
    let host_ip = host.and_then(|name| name.as_str().parse::<std::net::IpAddr>().ok());
    if host_ip.is_some() {
        return false;
    }
    !net.resolved_ips().is_empty()
        && net
            .ip_classes()
            .iter()
            .any(|class| *class != IpClass::Public)
}

fn glob_field_matches(pattern: Option<&PathGlob>, path: &str) -> bool {
    pattern.is_none_or(|glob| glob_matches(glob.as_str(), path))
}

fn absent_or_eq<T: PartialEq>(expected: Option<&T>, actual: &T) -> bool {
    expected.is_none_or(|want| want == actual)
}

fn absent_or_str(expected: Option<&str>, actual: &str) -> bool {
    expected.is_none_or(|want| want == actual)
}

fn rule_consideration(
    rule_effect: PolicyEffect,
    applied: Option<PolicyEffect>,
    layer_consideration: RuleConsideration,
    ceiling: Option<PolicyEffect>,
) -> RuleConsideration {
    if applied == Some(rule_effect) {
        return RuleConsideration::Applied;
    }
    if matches!(rule_effect, PolicyEffect::Allow | PolicyEffect::Ask)
        && layer_consideration == RuleConsideration::IgnoredCannotGrant
    {
        return RuleConsideration::IgnoredCannotGrant;
    }
    let broader_than_applied =
        restrictiveness(rule_effect) < restrictiveness_opt(applied.or(ceiling));
    if rule_effect != PolicyEffect::Deny && broader_than_applied {
        return RuleConsideration::IgnoredCannotBroaden;
    }
    if applied.is_some_and(|effect| rule_effect != effect) {
        return RuleConsideration::IgnoredCannotBroaden;
    }
    layer_consideration
}

fn net_host_matches(expected: &str, net: &CanonicalNetworkTarget) -> bool {
    match net.host() {
        crate::normalize::network::CanonicalNetHost::Dns(host) => host.as_str() == expected,
        crate::normalize::network::CanonicalNetHost::Ip(ip) => ip.to_string() == expected,
    }
}

fn layer_opinion(matches: &[&PolicyRule]) -> Option<PolicyEffect> {
    let mut saw_ask = false;
    let mut saw_allow = false;
    for rule in matches {
        match rule.effect() {
            PolicyEffect::Deny => return Some(PolicyEffect::Deny),
            PolicyEffect::Ask => saw_ask = true,
            PolicyEffect::Allow => saw_allow = true,
        }
    }
    if saw_ask {
        Some(PolicyEffect::Ask)
    } else if saw_allow {
        Some(PolicyEffect::Allow)
    } else {
        None
    }
}

fn consider_layer(
    layer: PolicyLayer,
    ceiling: Option<PolicyEffect>,
    opinion: Option<PolicyEffect>,
) -> (Option<PolicyEffect>, RuleConsideration) {
    match opinion {
        None => (None, RuleConsideration::Applied),
        Some(PolicyEffect::Deny) => (Some(PolicyEffect::Deny), RuleConsideration::Applied),
        Some(PolicyEffect::Allow) if !layer.may_grant() => {
            (None, RuleConsideration::IgnoredCannotGrant)
        }
        Some(PolicyEffect::Ask) if !layer.may_grant() && ceiling.is_none() => {
            (None, RuleConsideration::IgnoredCannotGrant)
        }
        Some(effect) => match ceiling {
            None => (Some(effect), RuleConsideration::Applied),
            Some(cur) if restrictiveness(effect) < restrictiveness(cur) => {
                (None, RuleConsideration::IgnoredCannotBroaden)
            }
            Some(_) => (Some(effect), RuleConsideration::Applied),
        },
    }
}

fn most_restrictive(a: PolicyEffect, b: PolicyEffect) -> PolicyEffect {
    if restrictiveness(a) >= restrictiveness(b) {
        a
    } else {
        b
    }
}

fn restrictiveness(effect: PolicyEffect) -> u8 {
    match effect {
        PolicyEffect::Allow => 0,
        PolicyEffect::Ask => 1,
        PolicyEffect::Deny => 2,
    }
}

fn restrictiveness_opt(effect: Option<PolicyEffect>) -> u8 {
    effect.map(restrictiveness).unwrap_or(2)
}

fn deny_reason(ceiling: Option<PolicyEffect>, deciding_layer: Option<PolicyLayer>) -> DenyReason {
    match (ceiling, deciding_layer) {
        (None, _) => DenyReason::DefaultDeny,
        (Some(PolicyEffect::Deny), Some(layer)) if layer.may_grant() => {
            if layer_rank(layer) <= layer_rank(PolicyLayer::User) {
                DenyReason::HigherTrustDeny
            } else {
                DenyReason::MatchedRule
            }
        }
        (Some(PolicyEffect::Deny), _) => DenyReason::MatchedRule,
        _ => DenyReason::DefaultDeny,
    }
}

fn layer_rank(layer: PolicyLayer) -> u8 {
    match layer {
        PolicyLayer::Compiled => 0,
        PolicyLayer::Organization => 1,
        PolicyLayer::User => 2,
        PolicyLayer::TrustedProject => 3,
        PolicyLayer::Session => 4,
    }
}

fn risk_class(capability: Capability, resource: &ResourceDescriptor) -> RiskClass {
    match capability {
        Capability::FsRead => match resource {
            ResourceDescriptor::Filesystem(scope) if scope.root() == FilesystemRoot::Repo => {
                RiskClass::Low
            }
            _ => RiskClass::Medium,
        },
        Capability::FsWrite => match resource {
            ResourceDescriptor::Filesystem(scope) if scope.root() == FilesystemRoot::Repo => {
                RiskClass::Medium
            }
            _ => RiskClass::High,
        },
        Capability::ProcExec
        | Capability::NetConnect
        | Capability::GitWrite
        | Capability::SecretUse
        | Capability::BrowserNavigate
        | Capability::BrowserDownload
        | Capability::MobileControl
        | Capability::McpInvoke
        | Capability::PluginInvoke => RiskClass::High,
    }
}

fn build_explanation(decision: &Decision, matched: &[MatchedRule]) -> String {
    let applied: Vec<&MatchedRule> = matched
        .iter()
        .filter(|rule| rule.considered == RuleConsideration::Applied)
        .collect();
    match decision {
        Decision::Allow(_) => {
            if let Some(rule) = applied
                .iter()
                .find(|rule| rule.effect == PolicyEffect::Allow)
            {
                format!("allow: {}:{}", rule.layer.as_str(), rule.id.as_str())
            } else {
                "allow".to_owned()
            }
        }
        Decision::Ask(_) => {
            if let Some(rule) = applied.iter().find(|rule| rule.effect == PolicyEffect::Ask) {
                format!("ask: {}:{}", rule.layer.as_str(), rule.id.as_str())
            } else {
                "ask".to_owned()
            }
        }
        Decision::Deny(reason) => {
            if let Some(rule) = applied
                .iter()
                .find(|rule| rule.effect == PolicyEffect::Deny)
            {
                match reason {
                    DenyReason::HigherTrustDeny => format!(
                        "deny: {}:{} (higher-trust deny is final)",
                        rule.layer.as_str(),
                        rule.id.as_str()
                    ),
                    _ => format!("deny: {}:{}", rule.layer.as_str(), rule.id.as_str()),
                }
            } else {
                format!("deny: {}", reason.as_str())
            }
        }
    }
}

/// Bounds total recursive calls across one `glob_matches` invocation.
///
/// Both `**` (segment level, `glob_match_segments`) and `*` (character
/// level, `glob_star_question`) backtrack by trying two branches per step —
/// a pattern or path that is one long run of either construct makes the
/// *total* number of calls across all branches blow up combinatorially
/// (backtracking over which of the other side's few remaining segments/
/// bytes each step consumes) long before any single call chain is deep
/// enough to threaten the stack on its own. `PathGlob`/`RepoPath` already
/// cap total bytes at 4096, but that alone does not bound this: a shared
/// call budget, decremented on every call and checked before recursing
/// further, bounds both total work and max depth (depth can never exceed
/// calls spent) in one guard, on every filesystem/git/browser permission
/// check this function backs. No real policy glob or requested path is
/// remotely close to exhausting it in genuine use — real matches resolve
/// in a handful of calls.
const MAX_GLOB_MATCH_CALLS: u32 = 10_000;

/// `*` matches within one path segment. `**` matches zero or more segments.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern_abs = pattern.starts_with('/');
    let path_abs = path.starts_with('/');
    if pattern_abs != path_abs {
        return false;
    }
    let pat: Vec<&str> = split_segments(pattern);
    let val: Vec<&str> = split_segments(path);
    let mut budget = MAX_GLOB_MATCH_CALLS;
    glob_match_segments(&pat, &val, &mut budget)
}

fn split_segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

fn glob_match_segments(pattern: &[&str], path: &[&str], budget: &mut u32) -> bool {
    let Some(remaining) = budget.checked_sub(1) else {
        return false;
    };
    *budget = remaining;
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&"**", rest)), None) => glob_match_segments(rest, path, budget),
        (Some((&"**", rest)), Some(_)) => {
            glob_match_segments(rest, path, budget)
                || glob_match_segments(pattern, &path[1..], budget)
        }
        (Some(_), None) => false,
        (Some((p, prest)), Some((v, vrest))) => {
            segment_matches(p, v, budget) && glob_match_segments(prest, vrest, budget)
        }
    }
}

fn segment_matches(pattern: &str, value: &str, budget: &mut u32) -> bool {
    if pattern == "*" || pattern == value {
        return true;
    }
    glob_star_question(pattern.as_bytes(), value.as_bytes(), budget)
}

fn glob_star_question(pattern: &[u8], value: &[u8], budget: &mut u32) -> bool {
    let Some(remaining) = budget.checked_sub(1) else {
        return false;
    };
    *budget = remaining;
    match (pattern.split_first(), value.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((b'*', rest)), None) => glob_star_question(rest, value, budget),
        (Some((b'*', rest)), Some(_)) => {
            glob_star_question(rest, value, budget)
                || glob_star_question(pattern, &value[1..], budget)
        }
        (Some(_), None) => false,
        (Some((b'?', prest)), Some((_, vrest))) => glob_star_question(prest, vrest, budget),
        (Some((p, prest)), Some((v, vrest))) if p == v => {
            glob_star_question(prest, vrest, budget)
        }
        (Some(_), Some(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{
        BrowserScope, FilesystemScope, GitScope, Hostname, McpScope, NetworkScheme, NetworkScope,
        Origin, PluginScope, ProcessScope, SecretScope,
    };
    use crate::policy::parser::PolicySource;

    const SECRET: &str = "super-secret-password";

    fn parse_doc(src: &str, source: PolicySource) -> PolicyDocument {
        PolicyDocument::parse_toml(src, source, &CancellationToken::new()).expect("parse")
    }

    fn compiled(src: &str) -> PolicyDocument {
        parse_doc(src, PolicySource::compiled())
    }

    fn org(src: &str) -> PolicyDocument {
        parse_doc(
            src,
            PolicySource::organization("org-policy.toml").expect("org"),
        )
    }

    fn user(src: &str) -> PolicyDocument {
        parse_doc(src, PolicySource::user("user-policy.toml").expect("user"))
    }

    fn project(src: &str) -> PolicyDocument {
        parse_doc(
            src,
            PolicySource::trusted_project(".rapidlm/policy.toml").expect("project"),
        )
    }

    fn session(src: &str) -> PolicyDocument {
        parse_doc(src, PolicySource::session("session").expect("session"))
    }

    fn stack(docs: impl IntoIterator<Item = PolicyDocument>) -> PolicyStack {
        PolicyStack::new(docs).expect("stack")
    }

    fn principal() -> PrincipalRef {
        PrincipalRef::parse("agent").expect("principal")
    }

    fn repo_read_resource() -> ResourceDescriptor {
        ResourceDescriptor::Filesystem(FilesystemScope::repo("src/main.rs").expect("fs"))
    }

    fn repo_read_action() -> CanonicalAction {
        CanonicalAction::Resource {
            capability: Capability::FsRead,
            resource: repo_read_resource(),
        }
    }

    fn repo_read_request() -> ActionRequest {
        ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "read source",
        )
        .expect("request")
    }

    fn eval(stack: &PolicyStack, request: &ActionRequest) -> DecisionWithTrace {
        evaluate(stack, request, &CancellationToken::new()).expect("evaluate")
    }

    fn rule_src(id: &str, effect: &str) -> String {
        format!(
            r#"
[[rules]]
id = "{id}"
effect = "{effect}"
subjects = ["*"]
capability = "fs.read"
resource = {{ root = "repo", glob = "src/**" }}
"#
        )
    }

    fn restrictiveness_of(trace: &DecisionWithTrace) -> u8 {
        restrictiveness(trace.effect())
    }

    #[test]
    fn evaluate_is_pure_for_same_inputs() {
        let policies = stack([
            user(&rule_src("repo-read", "allow")),
            project(&rule_src("tighten", "ask")),
        ]);
        let request = repo_read_request();
        let a = eval(&policies, &request);
        let b = eval(&policies, &request);
        assert_eq!(a, b);
        assert_eq!(a.effect(), PolicyEffect::Ask);
        assert_eq!(a.explanation(), b.explanation());
        assert_eq!(
            a.matched_rule_ids().collect::<Vec<_>>(),
            b.matched_rule_ids().collect::<Vec<_>>()
        );
    }

    #[test]
    fn layer_precedence_truth_table() {
        let request = repo_read_request();
        let cases: &[(&str, Vec<PolicyDocument>, PolicyEffect)] = &[
            ("empty", vec![], PolicyEffect::Deny),
            (
                "user-allow",
                vec![user(&rule_src("u-allow", "allow"))],
                PolicyEffect::Allow,
            ),
            (
                "compiled-deny-over-user-allow",
                vec![
                    compiled(&rule_src("c-deny", "deny")),
                    user(&rule_src("u-allow", "allow")),
                ],
                PolicyEffect::Deny,
            ),
            (
                "org-deny-over-user-allow",
                vec![
                    org(&rule_src("o-deny", "deny")),
                    user(&rule_src("u-allow", "allow")),
                ],
                PolicyEffect::Deny,
            ),
            (
                "user-allow-project-deny",
                vec![
                    user(&rule_src("u-allow", "allow")),
                    project(&rule_src("p-deny", "deny")),
                ],
                PolicyEffect::Deny,
            ),
            (
                "user-allow-project-ask",
                vec![
                    user(&rule_src("u-allow", "allow")),
                    project(&rule_src("p-ask", "ask")),
                ],
                PolicyEffect::Ask,
            ),
            (
                "user-ask-project-deny",
                vec![
                    user(&rule_src("u-ask", "ask")),
                    project(&rule_src("p-deny", "deny")),
                ],
                PolicyEffect::Deny,
            ),
            (
                "user-allow-session-ask",
                vec![
                    user(&rule_src("u-allow", "allow")),
                    session(&rule_src("s-ask", "ask")),
                ],
                PolicyEffect::Ask,
            ),
            (
                "compiled-allow-user-deny",
                vec![
                    compiled(&rule_src("c-allow", "allow")),
                    user(&rule_src("u-deny", "deny")),
                ],
                PolicyEffect::Deny,
            ),
            (
                "project-ask-without-grant",
                vec![project(&rule_src("p-ask", "ask"))],
                PolicyEffect::Deny,
            ),
            (
                "session-deny-without-grant",
                vec![session(&rule_src("s-deny", "deny"))],
                PolicyEffect::Deny,
            ),
        ];
        for (name, docs, expected) in cases {
            let policies = stack(docs.clone());
            let got = eval(&policies, &request);
            assert_eq!(got.effect(), *expected, "{name}");
        }
    }

    #[test]
    fn property_adding_deny_or_restriction_cannot_broaden() {
        let grant_effects = [None, Some("allow"), Some("ask"), Some("deny")];
        let restrict_effects = [None, Some("ask"), Some("deny")];
        let request = repo_read_request();
        let mut cases = 0u32;

        for compiled_e in grant_effects {
            for org_e in grant_effects {
                for user_e in grant_effects {
                    for project_e in restrict_effects {
                        for session_e in restrict_effects {
                            let mut docs = Vec::new();
                            if let Some(effect) = compiled_e {
                                docs.push(compiled(&rule_src("c", effect)));
                            }
                            if let Some(effect) = org_e {
                                docs.push(org(&rule_src("o", effect)));
                            }
                            if let Some(effect) = user_e {
                                docs.push(user(&rule_src("u", effect)));
                            }
                            if let Some(effect) = project_e {
                                docs.push(project(&rule_src("p", effect)));
                            }
                            if let Some(effect) = session_e {
                                docs.push(session(&rule_src("s", effect)));
                            }
                            let base_stack = stack(docs.clone());
                            let base = restrictiveness_of(&eval(&base_stack, &request));

                            for extra in [
                                compiled(&rule_src("c-extra-deny", "deny")),
                                org(&rule_src("o-extra-deny", "deny")),
                                user(&rule_src("u-extra-deny", "deny")),
                                project(&rule_src("p-extra-deny", "deny")),
                                project(&rule_src("p-extra-ask", "ask")),
                                session(&rule_src("s-extra-deny", "deny")),
                                session(&rule_src("s-extra-ask", "ask")),
                            ] {
                                let mut restricted = docs.clone();
                                restricted.push(extra);
                                let next = restrictiveness_of(&eval(&stack(restricted), &request));
                                assert!(
                                    next >= base,
                                    "restriction broadened {base} -> {next} compiled={compiled_e:?} org={org_e:?} user={user_e:?} project={project_e:?} session={session_e:?}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(cases >= 4 * 4 * 4 * 3 * 3);
    }

    #[test]
    fn dont_ask_turns_ask_into_deny_and_leaves_other_decisions() {
        let ask = Decision::Ask(AskExplanation {
            rule_ids: Vec::new(),
        });
        assert_eq!(
            dont_ask(ask.clone()),
            Decision::Deny(DenyReason::NonInteractive)
        );
        assert_eq!(
            dont_ask(Decision::Allow(LeaseConstraints::standard())),
            Decision::Allow(LeaseConstraints::standard())
        );
        assert_eq!(
            dont_ask(Decision::Deny(DenyReason::DefaultDeny)),
            Decision::Deny(DenyReason::DefaultDeny)
        );
        // The transformed decision is a hard deny, never a prompt.
        assert_eq!(dont_ask(ask).effect(), PolicyEffect::Deny);
    }

    #[test]
    fn decision_includes_matched_rule_ids_without_secret_data() {
        let policies = stack([
            user(
                r#"
[[rules]]
id = "secret-allow"
effect = "allow"
capability = "secret.use"
resource = { secret_id = "env:NPM_TOKEN", target = "env" }
"#,
            ),
            project(
                r#"
[[rules]]
id = "secret-ask"
effect = "ask"
capability = "secret.use"
"#,
            ),
        ]);
        let resource = ResourceDescriptor::Secret(
            SecretScope::new("env:NPM_TOKEN", "env").expect("secret scope"),
        );
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::SecretUse,
            resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::SecretUse,
                resource,
            },
            SECRET,
        )
        .expect("request");
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Ask);
        let ids: Vec<_> = got.matched_rule_ids().collect();
        assert_eq!(ids, ["secret-allow", "secret-ask"]);
        let explain = got.explanation();
        assert!(explain.contains("secret-ask"));
        assert!(!explain.contains(SECRET));
        assert!(!explain.contains("NPM_TOKEN"));
        assert!(!format!("{got:?}").contains(SECRET));
        assert!(!got.decision().effect().as_str().is_empty());
    }

    #[test]
    fn project_and_session_cannot_broaden_higher_trust_deny() {
        let request = repo_read_request();
        let policies = stack([
            user(&rule_src("u-deny", "deny")),
            project(&rule_src("p-ask", "ask")),
            session(&rule_src("s-ask", "ask")),
        ]);
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert!(matches!(
            got.decision(),
            Decision::Deny(DenyReason::HigherTrustDeny | DenyReason::MatchedRule)
        ));
        assert!(
            got.explanation().contains("higher-trust deny is final")
                || got.explanation().contains("u-deny")
        );
    }

    #[test]
    fn subject_mismatch_cannot_use_other_principal_allow() {
        let policies = stack([user(
            r#"
[[rules]]
id = "other-allow"
effect = "allow"
subjects = ["other-agent"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
        )]);
        let got = eval(&policies, &repo_read_request());
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert!(got.matched_rules().is_empty());
        assert_eq!(got.decision(), &Decision::Deny(DenyReason::DefaultDeny));
    }

    #[test]
    fn capability_mismatch_cannot_use_read_allow_for_write() {
        let policies = stack([user(&rule_src("repo-read", "allow"))]);
        let resource =
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/main.rs").expect("fs"));
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsWrite,
            resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::FsWrite,
                resource,
            },
            "overwrite",
        )
        .expect("request");
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert!(got.matched_rules().is_empty());
    }

    #[test]
    fn resource_glob_does_not_match_outside_path() {
        let policies = stack([user(&rule_src("repo-read", "allow"))]);
        let resource =
            ResourceDescriptor::Filesystem(FilesystemScope::repo("tests/main.rs").expect("fs"));
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsRead,
            resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::FsRead,
                resource,
            },
            "read tests",
        )
        .expect("request");
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert!(got.matched_rules().is_empty());
    }

    #[test]
    fn host_write_not_covered_by_repo_allow() {
        let policies = stack([user(
            r#"
[[rules]]
id = "repo-write"
effect = "allow"
capability = "fs.write"
resource = { root = "repo", glob = "src/**" }
"#,
        )]);
        let resource =
            ResourceDescriptor::Filesystem(FilesystemScope::host("/tmp/out").expect("host"));
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsWrite,
            resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::FsWrite,
                resource,
            },
            "host write",
        )
        .expect("request");
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert!(got.matched_rules().is_empty());
    }

    #[test]
    fn network_host_mismatch_is_denied() {
        let policies = stack([user(
            r#"
[[rules]]
id = "net-allow"
effect = "allow"
capability = "net.connect"
resource = { scheme = "https", host = "example.com", port = 443 }
"#,
        )]);
        let resource = ResourceDescriptor::Network(
            NetworkScope::new(NetworkScheme::Https, "evil.example", 443).expect("net"),
        );
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::NetConnect,
            resource.clone(),
            CanonicalAction::Resource {
                capability: Capability::NetConnect,
                resource,
            },
            "ssrf",
        )
        .expect("request");
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert!(got.matched_rules().is_empty());
    }

    #[test]
    fn incompatible_capability_and_resource_is_deny() {
        let policies = stack([user(&rule_src("repo-read", "allow"))]);
        let resource = ResourceDescriptor::Network(
            NetworkScope::new(NetworkScheme::Https, "example.com", 443).expect("net"),
        );
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsRead,
            resource,
            repo_read_action(),
            "mismatch",
        )
        .expect("request");
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert_eq!(
            got.decision(),
            &Decision::Deny(DenyReason::IncompatibleAction)
        );
        assert!(got.matched_rules().is_empty());
        assert_eq!(got.explanation(), "deny: incompatible-action");
    }

    #[test]
    fn within_layer_deny_beats_allow() {
        let policies = stack([user(
            r#"
[[rules]]
id = "allow-src"
effect = "allow"
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }

[[rules]]
id = "deny-src"
effect = "deny"
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#,
        )]);
        let got = eval(&policies, &repo_read_request());
        assert_eq!(got.effect(), PolicyEffect::Deny);
        let ids: Vec<_> = got.matched_rule_ids().collect();
        assert_eq!(ids, ["allow-src", "deny-src"]);
    }

    #[test]
    fn default_allow_lease_constraints_are_one_use_and_short_ttl() {
        let policies = stack([user(&rule_src("repo-read", "allow"))]);
        let got = eval(&policies, &repo_read_request());
        match got.decision() {
            Decision::Allow(constraints) => {
                assert_eq!(constraints.max_uses(), 1);
                assert!(constraints.max_ttl_secs() <= 60);
            }
            other => panic!("expected allow, got {other:?}"),
        }
        assert_eq!(got.risk(), RiskClass::Low);
    }

    #[test]
    fn cancellation_fails_closed() {
        let policies = stack([user(&rule_src("repo-read", "allow"))]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            evaluate(&policies, &repo_read_request(), &cancel),
            Err(PolicyEvalError::Cancelled)
        );
        assert!(!PolicyEvalError::Cancelled.to_string().contains(SECRET));
    }

    #[test]
    fn oversized_stack_and_invalid_principal_fail_closed() {
        let docs = (0..MAX_POLICY_STACK_DOCUMENTS + 1)
            .map(|_| user(&rule_src("repo-read", "allow")))
            .collect::<Vec<_>>();
        assert_eq!(
            PolicyStack::new(docs).unwrap_err(),
            PolicyEvalError::StackTooLarge
        );
        assert_eq!(
            PrincipalRef::parse(""),
            Err(PolicyEvalError::InvalidPrincipal)
        );
        assert_eq!(
            PrincipalRef::parse("super secret password"),
            Err(PolicyEvalError::InvalidPrincipal)
        );
        assert_eq!(
            PrincipalRef::parse("sk-canary!value"),
            Err(PolicyEvalError::InvalidPrincipal)
        );
        let err = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsRead,
            repo_read_resource(),
            repo_read_action(),
            "x".repeat(MAX_REASON_BYTES + 1),
        )
        .unwrap_err();
        assert_eq!(err, PolicyEvalError::ReasonTooLarge);
        assert!(!err.to_string().contains(SECRET));
    }

    #[test]
    fn glob_matching_covers_double_star_and_rejects_escape() {
        assert!(glob_matches("src/**", "src/main.rs"));
        assert!(glob_matches("src/**", "src/foo/bar.rs"));
        assert!(glob_matches("**/*.rs", "src/main.rs"));
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/foo/bar.rs"));
        assert!(!glob_matches("src/**", "tests/main.rs"));
        assert!(!glob_matches("/tmp/**", "tmp/out"));
        assert!(glob_matches("/tmp/**", "/tmp/out"));
        assert!(glob_matches("refs/heads/*", "refs/heads/main"));
        assert!(!glob_matches("refs/heads/*", "refs/tags/v1"));
    }

    #[test]
    fn glob_matching_bounds_pathological_backtracking_on_every_permission_check() {
        // A long run of `*`/`**` is redundant but not pathological on its
        // own — still just "match anything" and must resolve correctly.
        let redundant_stars = "*".repeat(4096);
        assert!(glob_matches(&redundant_stars, "short.rs"));
        let redundant_double_stars = std::iter::repeat_n("**", 2048)
            .collect::<Vec<_>>()
            .join("/");
        assert!(glob_match_segments(
            &split_segments(&redundant_double_stars),
            &split_segments("a/b/c.rs"),
            &mut MAX_GLOB_MATCH_CALLS.saturating_mul(2),
        ));

        // A pattern that can *never* match (an impossible trailing literal)
        // is the actually pathological case: naive backtracking without a
        // call budget explores a combinatorial number of ways to interleave
        // "consume a star/`**`" against "consume a segment/byte" before
        // concluding no match is possible — on this file's real production
        // path (every filesystem/git/browser permission check), that would
        // hang the decision instead of just risking the stack. The call
        // budget must make both resolve to `false` quickly.
        let unmatchable_chars = format!("{}Z", "*".repeat(4096));
        assert!(!glob_matches(&unmatchable_chars, "short.rs"));
        let unmatchable_segments = format!(
            "{}/z",
            std::iter::repeat_n("**", 2048).collect::<Vec<_>>().join("/")
        );
        assert!(!glob_match_segments(
            &split_segments(&unmatchable_segments),
            &split_segments("a/b/c.rs"),
            &mut MAX_GLOB_MATCH_CALLS.saturating_mul(2),
        ));
    }

    #[test]
    fn allow_covers_other_families_without_leaking_values() {
        let policies = stack([org(r#"
[[rules]]
id = "mcp-allow"
effect = "allow"
capability = "mcp.invoke"
resource = { server = "github", tool = "create_issue" }

[[rules]]
id = "plugin-allow"
effect = "allow"
capability = "plugin.invoke"
resource = { plugin = "formatter", capability = "format" }

[[rules]]
id = "git-allow"
effect = "allow"
capability = "git.write"
resource = { ref_scope = "refs/heads/*" }

[[rules]]
id = "proc-allow"
effect = "allow"
capability = "proc.exec"
resource = { command_family = "cargo" }

[[rules]]
id = "browser-allow"
effect = "allow"
capability = "browser.navigate"
resource = { origin = "https://example.com" }
"#)]);

        let mcp = ResourceDescriptor::Mcp(McpScope::new("github", "create_issue").expect("mcp"));
        let mcp_req = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::McpInvoke,
            mcp.clone(),
            CanonicalAction::Resource {
                capability: Capability::McpInvoke,
                resource: mcp,
            },
            "mcp",
        )
        .expect("mcp req");
        assert_eq!(eval(&policies, &mcp_req).effect(), PolicyEffect::Allow);

        let plugin =
            ResourceDescriptor::Plugin(PluginScope::new("formatter", "format").expect("plugin"));
        let plugin_req = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::PluginInvoke,
            plugin.clone(),
            CanonicalAction::Resource {
                capability: Capability::PluginInvoke,
                resource: plugin,
            },
            "plugin",
        )
        .expect("plugin req");
        assert_eq!(eval(&policies, &plugin_req).effect(), PolicyEffect::Allow);

        let git = ResourceDescriptor::Git(GitScope::new("refs/heads/main").expect("git"));
        let git_req = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::GitWrite,
            git.clone(),
            CanonicalAction::Resource {
                capability: Capability::GitWrite,
                resource: git,
            },
            "git",
        )
        .expect("git req");
        assert_eq!(eval(&policies, &git_req).effect(), PolicyEffect::Allow);

        let proc = ResourceDescriptor::Process(ProcessScope::new("cargo").expect("proc"));
        let proc_req = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::ProcExec,
            proc.clone(),
            CanonicalAction::Resource {
                capability: Capability::ProcExec,
                resource: proc,
            },
            "proc",
        )
        .expect("proc req");
        assert_eq!(eval(&policies, &proc_req).effect(), PolicyEffect::Allow);

        let browser = ResourceDescriptor::Browser(BrowserScope::navigate(
            Origin::parse("https://example.com").expect("origin"),
        ));
        let browser_req = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::BrowserNavigate,
            browser.clone(),
            CanonicalAction::Resource {
                capability: Capability::BrowserNavigate,
                resource: browser,
            },
            "browser",
        )
        .expect("browser req");
        assert_eq!(eval(&policies, &browser_req).effect(), PolicyEffect::Allow);

        let other_mcp = ResourceDescriptor::Mcp(McpScope::new("evil", "exfil").expect("mcp"));
        let other_req = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::McpInvoke,
            other_mcp.clone(),
            CanonicalAction::Resource {
                capability: Capability::McpInvoke,
                resource: other_mcp,
            },
            "bypass",
        )
        .expect("other mcp");
        assert_eq!(eval(&policies, &other_req).effect(), PolicyEffect::Deny);
        let _ = Hostname::parse("example.com");
    }

    #[test]
    fn empty_stack_is_default_deny() {
        let got = eval(&PolicyStack::empty(), &repo_read_request());
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert_eq!(got.decision(), &Decision::Deny(DenyReason::DefaultDeny));
        assert_eq!(got.explanation(), "deny: default-deny");
        assert!(got.matched_rules().is_empty());
    }

    struct TestRepoFs {
        repo: crate::normalize::command::CanonicalHostPath,
        host: crate::normalize::command::CanonicalHostPath,
    }

    impl TestRepoFs {
        fn new() -> Self {
            Self {
                repo: crate::normalize::command::CanonicalHostPath::from_resolved("/repo")
                    .expect("repo"),
                host: crate::normalize::command::CanonicalHostPath::from_resolved("/home/user")
                    .expect("host"),
            }
        }
    }

    impl crate::normalize::fs::FsResolver for TestRepoFs {
        fn repo_root(&self) -> &crate::normalize::command::CanonicalHostPath {
            &self.repo
        }

        fn host_base(&self) -> &crate::normalize::command::CanonicalHostPath {
            &self.host
        }

        fn exists(
            &self,
            path: &crate::normalize::command::CanonicalHostPath,
        ) -> Result<bool, crate::normalize::fs::FsNormalizeError> {
            Ok(matches!(
                path.as_str(),
                "/repo" | "/repo/src" | "/repo/src/secret.rs" | "/repo/public"
            ))
        }

        fn is_dir(
            &self,
            path: &crate::normalize::command::CanonicalHostPath,
        ) -> Result<bool, crate::normalize::fs::FsNormalizeError> {
            Ok(matches!(
                path.as_str(),
                "/" | "/repo" | "/repo/src" | "/repo/public"
            ))
        }

        fn read_link(
            &self,
            _path: &crate::normalize::command::CanonicalHostPath,
        ) -> Result<Option<String>, crate::normalize::fs::FsNormalizeError> {
            Ok(None)
        }
    }

    fn rename_src_to_public() -> CanonicalAction {
        let action = crate::normalize::fs::normalize_fs(
            &crate::normalize::fs::FsIntent::rename(
                FilesystemRoot::Repo,
                "src/secret.rs",
                "public/leaked.rs",
            ),
            &TestRepoFs::new(),
            &CancellationToken::new(),
        )
        .expect("rename normalize");
        CanonicalAction::Filesystem(action)
    }

    fn repo_write_request(action: CanonicalAction, glob: &str) -> ActionRequest {
        ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::FsWrite,
            ResourceDescriptor::Filesystem(FilesystemScope::repo(glob).expect("fs")),
            action,
            "rename",
        )
        .expect("request")
    }

    #[test]
    fn rename_cannot_escape_src_deny_via_unrestricted_dest() {
        let action = rename_src_to_public();
        assert_eq!(
            match &action {
                CanonicalAction::Filesystem(fs) => fs.path().as_str(),
                _ => "",
            },
            "src/secret.rs"
        );
        assert_eq!(
            match &action {
                CanonicalAction::Filesystem(fs) => fs.dest().map(|dest| dest.as_str()),
                _ => None,
            },
            Some("public/leaked.rs")
        );

        let grant = user(
            r#"
[[rules]]
id = "write-all"
effect = "allow"
subjects = ["*"]
capability = "fs.write"
resource = { root = "repo", glob = "**" }
"#,
        );
        let project_deny = project(
            r#"
[[rules]]
id = "deny-src"
effect = "deny"
subjects = ["*"]
capability = "fs.write"
resource = { root = "repo", glob = "src/**" }
"#,
        );
        let session_deny = session(
            r#"
[[rules]]
id = "deny-src-session"
effect = "deny"
subjects = ["*"]
capability = "fs.write"
resource = { root = "repo", glob = "src/**" }
"#,
        );

        for (name, docs, glob) in [
            (
                "project-deny-source-resource",
                vec![grant.clone(), project_deny.clone()],
                "src/secret.rs",
            ),
            (
                "session-deny-source-resource",
                vec![grant.clone(), session_deny.clone()],
                "src/secret.rs",
            ),
            (
                "project-deny-dest-resource",
                vec![grant.clone(), project_deny],
                "public/leaked.rs",
            ),
            (
                "session-deny-dest-resource",
                vec![grant, session_deny],
                "public/leaked.rs",
            ),
        ] {
            let got = eval(&stack(docs), &repo_write_request(action.clone(), glob));
            assert_eq!(got.effect(), PolicyEffect::Deny, "{name}");
        }
    }

    struct RebindDns;

    impl crate::normalize::network::NetworkResolver for RebindDns {
        fn resolve(
            &self,
            _host: &Hostname,
        ) -> Result<Vec<std::net::IpAddr>, crate::normalize::network::NetworkNormalizeError>
        {
            Ok(vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                169, 254, 169, 254,
            ))])
        }
    }

    #[test]
    fn dns_rebind_metadata_like_is_denied() {
        let policies = stack([user(
            r#"
[[rules]]
id = "net-allow"
effect = "allow"
subjects = ["*"]
capability = "net.connect"
resource = { scheme = "https", host = "example.com", port = 443 }
"#,
        )]);
        let target = crate::normalize::network::normalize_network(
            &crate::normalize::network::NetworkIntent::connect("https://example.com/"),
            &RebindDns,
            &CancellationToken::new(),
        )
        .expect("normalize");
        assert!(target.ip_classes().contains(&IpClass::MetadataLike));
        let resource = ResourceDescriptor::Network(
            NetworkScope::new(NetworkScheme::Https, "example.com", 443).expect("net"),
        );
        let request = ActionRequest::new(
            principal(),
            SessionId::new(),
            Capability::NetConnect,
            resource,
            CanonicalAction::Network(target),
            "dns-rebind",
        )
        .expect("request");
        let got = eval(&policies, &request);
        assert_eq!(got.effect(), PolicyEffect::Deny);
        assert!(
            matches!(
                got.decision(),
                Decision::Deny(DenyReason::DefaultDeny | DenyReason::IncompatibleAction)
            ),
            "{:?}",
            got.decision()
        );
    }
}
