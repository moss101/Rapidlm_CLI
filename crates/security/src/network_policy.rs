//! Egress policy proxy for sandbox/browser/tool connects.
//!
//! `authorize_connect` freezes the canonical origin + resolved IPs, applies
//! allow/deny rules, and issues a one-use connect lease. DNS answers are
//! revalidated at consume time. Every redirect hop is a new authorization.
//! Loopback/private/link-local/metadata targets are denied unless an explicit
//! rule covers them. Privilege uncertainty and resolver failure are errors,
//! never Allow. Threats: `T-005`, `T-001`.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use capability_broker::{
    CancellationToken, CanonicalNetHost, CanonicalNetworkTarget, Hostname, IpClass, NetworkIntent,
    NetworkNormalizeError, NetworkResolver, NetworkScheme, normalize_network,
};

/// Maximum allowlist rules on one [`EgressPolicy`].
pub const MAX_EGRESS_RULES: usize = 256;

/// In-memory audit ring bound. Oldest records are dropped.
pub const MAX_AUDIT_RECORDS: usize = 256;

const CANCEL_STRIDE: usize = 8;
const INHERENT_LOOPBACK: &[&str] = &[
    "localhost",
    "localhost.localdomain",
    "ip6-localhost",
    "ip6-loopback",
];
const INHERENT_METADATA: &[&str] = &[
    "metadata",
    "metadata.google.internal",
    "metadata.internal",
    "instance-data",
];

/// Who is requesting the connect. Attribution only; never grants privilege.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NetworkClient {
    Sandbox,
    Browser,
    Tool,
    /// A model provider's endpoint.
    Provider,
}

/// Sandbox/browser/tool network mode. Isolated default is [`EgressMode::None`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EgressMode {
    None,
    Allowlist,
    Proxy,
}

/// Allowlist host: exact DNS name or literal IP after canonicalization.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum EgressHost {
    Dns(Hostname),
    Ip(IpAddr),
}

/// One allow rule. Missing scheme/port matches any closed scheme/port.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EgressRule {
    scheme: Option<NetworkScheme>,
    host: EgressHost,
    port: Option<u16>,
}

/// Enforceable egress envelope. Empty allowlists deny every connect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressPolicy {
    mode: EgressMode,
    rules: Vec<EgressRule>,
}

/// In-process egress proxy. Issues one-use connect leases and audit records.
pub struct EgressProxy {
    policy: EgressPolicy,
    client: NetworkClient,
    next_lease: AtomicU64,
    leases: Mutex<HashMap<u64, LeaseState>>,
    audit: Mutex<VecDeque<EgressAuditRecord>>,
}

/// Allow or policy-deny. Errors never become this type.
#[derive(Debug)]
pub enum EgressOutcome<T> {
    Allow(T),
    Deny(EgressDenial),
}

/// One-use connect permit bound to a canonical target and resolved IPs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectLease {
    id: ConnectLeaseId,
    target: CanonicalNetworkTarget,
    bound_ips: BTreeSet<IpAddr>,
}

/// Opaque lease identity. Not a capability lease token.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ConnectLeaseId(u64);

/// Proof a lease was consumed immediately before dial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsumedConnect {
    id: ConnectLeaseId,
    target: CanonicalNetworkTarget,
    dial_ips: BTreeSet<IpAddr>,
}

/// Policy denial. Distinct from [`EgressError`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressDenial {
    reason: EgressReason,
    audit: EgressAuditRecord,
}

/// Why a connect was denied. Display never echoes the raw URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EgressReason {
    NetworkDisabled,
    NotAllowlisted,
    SensitiveClass,
    RedirectForbidden,
    DnsRebind,
    TargetDrift,
}

/// Bounded audit row. Host/IP fields are canonical, never the raw URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressAuditRecord {
    decision: AuditDecision,
    reason: Option<EgressReason>,
    client: NetworkClient,
    mode: EgressMode,
    scheme: Option<NetworkScheme>,
    host: Option<String>,
    port: Option<u16>,
    ip_classes: BTreeSet<IpClass>,
    hop: u8,
    previous_host: Option<String>,
}

/// Typed proxy/normalize/lease failure. Never an Allow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EgressError {
    Cancelled,
    Normalize(NetworkNormalizeError),
    Unavailable,
    UsesExhausted,
    UnknownLease,
    InvalidRule,
    TooManyRules,
    InvalidPort,
    EmptyHost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum AuditDecision {
    Allow,
    Deny,
    Error,
}

struct LeaseState {
    target: CanonicalNetworkTarget,
    bound_ips: BTreeSet<IpAddr>,
    remaining: u32,
}

/// Validate the canonical target and return a one-use connect lease or deny.
pub fn authorize_connect<R: NetworkResolver + ?Sized>(
    proxy: &EgressProxy,
    intent: &NetworkIntent,
    resolver: &R,
    cancel: &CancellationToken,
) -> Result<EgressOutcome<ConnectLease>, EgressError> {
    proxy.authorize_connect(intent, resolver, cancel)
}

impl NetworkClient {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Browser => "browser",
            Self::Tool => "tool",
            Self::Provider => "provider",
        }
    }
}

impl EgressMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Allowlist => "allowlist",
            Self::Proxy => "proxy",
        }
    }
}

impl EgressHost {
    pub fn parse(raw: &str) -> Result<Self, EgressError> {
        if raw.is_empty() {
            return Err(EgressError::EmptyHost);
        }
        let unbracketed = raw
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(raw);
        if let Ok(ip) = unbracketed.parse::<IpAddr>() {
            return Ok(Self::Ip(canonical_ip(ip)));
        }
        match Hostname::parse(raw) {
            Ok(host) => Ok(Self::Dns(host)),
            Err(_) => Err(EgressError::InvalidRule),
        }
    }

    pub fn as_canonical_str(&self) -> String {
        match self {
            Self::Dns(host) => host.as_str().to_owned(),
            Self::Ip(ip) => ip.to_string(),
        }
    }
}

impl EgressRule {
    pub fn host(host: &str) -> Result<Self, EgressError> {
        Ok(Self {
            scheme: None,
            host: EgressHost::parse(host)?,
            port: None,
        })
    }

    pub fn host_port(host: &str, port: u16) -> Result<Self, EgressError> {
        if port == 0 {
            return Err(EgressError::InvalidPort);
        }
        Ok(Self {
            scheme: None,
            host: EgressHost::parse(host)?,
            port: Some(port),
        })
    }

    pub fn exact(scheme: NetworkScheme, host: &str, port: u16) -> Result<Self, EgressError> {
        if port == 0 {
            return Err(EgressError::InvalidPort);
        }
        Ok(Self {
            scheme: Some(scheme),
            host: EgressHost::parse(host)?,
            port: Some(port),
        })
    }

    pub fn scheme(&self) -> Option<NetworkScheme> {
        self.scheme
    }

    pub fn host_ref(&self) -> &EgressHost {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }
}

impl EgressPolicy {
    pub fn none() -> Self {
        Self {
            mode: EgressMode::None,
            rules: Vec::new(),
        }
    }

    pub fn allowlist(rules: impl IntoIterator<Item = EgressRule>) -> Result<Self, EgressError> {
        Self::with_mode(EgressMode::Allowlist, rules)
    }

    pub fn proxy(rules: impl IntoIterator<Item = EgressRule>) -> Result<Self, EgressError> {
        Self::with_mode(EgressMode::Proxy, rules)
    }

    fn with_mode(
        mode: EgressMode,
        rules: impl IntoIterator<Item = EgressRule>,
    ) -> Result<Self, EgressError> {
        let rules: Vec<EgressRule> = rules.into_iter().collect();
        if rules.len() > MAX_EGRESS_RULES {
            return Err(EgressError::TooManyRules);
        }
        Ok(Self { mode, rules })
    }

    pub fn mode(&self) -> EgressMode {
        self.mode
    }

    pub fn rules(&self) -> &[EgressRule] {
        &self.rules
    }
}

impl EgressProxy {
    pub fn new(policy: EgressPolicy, client: NetworkClient) -> Self {
        Self {
            policy,
            client,
            next_lease: AtomicU64::new(1),
            leases: Mutex::new(HashMap::new()),
            audit: Mutex::new(VecDeque::new()),
        }
    }

    pub fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    pub fn client(&self) -> NetworkClient {
        self.client
    }

    /// Recent audit records, oldest first. Poisoned lock is Unavailable.
    pub fn audit_log(&self) -> Result<Vec<EgressAuditRecord>, EgressError> {
        let guard = self.audit.lock().map_err(|_| EgressError::Unavailable)?;
        Ok(guard.iter().cloned().collect())
    }

    /// Normalize, apply allow/deny, return a one-use lease or a policy deny.
    pub fn authorize_connect<R: NetworkResolver + ?Sized>(
        &self,
        intent: &NetworkIntent,
        resolver: &R,
        cancel: &CancellationToken,
    ) -> Result<EgressOutcome<ConnectLease>, EgressError> {
        cancel_check(cancel)?;
        let target = match normalize_network(intent, resolver, cancel) {
            Ok(target) => target,
            Err(NetworkNormalizeError::Cancelled) => return Err(EgressError::Cancelled),
            Err(err) => {
                self.record_error_audit(intent, None)?;
                return Err(EgressError::Normalize(err));
            }
        };
        cancel_check(cancel)?;
        match decide(&self.policy, &target, intent.previous().is_some(), cancel)? {
            Decision::Allow => {
                let lease = self.issue_lease(target)?;
                self.record_audit(allow_audit(self.client, self.policy.mode, lease.target()))?;
                Ok(EgressOutcome::Allow(lease))
            }
            Decision::Deny(reason) => {
                let denial = EgressDenial {
                    audit: deny_audit(self.client, self.policy.mode, &target, reason),
                    reason,
                };
                self.record_audit(denial.audit.clone())?;
                Ok(EgressOutcome::Deny(denial))
            }
        }
    }

    /// Follow `Location` as a new action. Newly forbidden origin/IP is denied.
    pub fn authorize_redirect<R: NetworkResolver + ?Sized>(
        &self,
        previous: &CanonicalNetworkTarget,
        location: &str,
        resolver: &R,
        cancel: &CancellationToken,
    ) -> Result<EgressOutcome<ConnectLease>, EgressError> {
        cancel_check(cancel)?;
        let intent = match NetworkIntent::redirect(previous, location) {
            Ok(intent) => intent,
            Err(NetworkNormalizeError::Cancelled) => return Err(EgressError::Cancelled),
            Err(err) => {
                self.record_error_audit_from_target(previous, true)?;
                return Err(EgressError::Normalize(err));
            }
        };
        match self.authorize_connect(&intent, resolver, cancel)? {
            EgressOutcome::Allow(lease) => Ok(EgressOutcome::Allow(lease)),
            EgressOutcome::Deny(mut denial) => {
                if denial.reason == EgressReason::NotAllowlisted
                    || denial.reason == EgressReason::SensitiveClass
                    || denial.reason == EgressReason::NetworkDisabled
                {
                    denial.reason = EgressReason::RedirectForbidden;
                    denial.audit.reason = Some(EgressReason::RedirectForbidden);
                    denial.audit.decision = AuditDecision::Deny;
                    self.replace_last_audit(denial.audit.clone())?;
                }
                Ok(EgressOutcome::Deny(denial))
            }
        }
    }

    /// Burn the one-use lease and re-resolve DNS before dial.
    pub fn consume_connect<R: NetworkResolver + ?Sized>(
        &self,
        lease: &ConnectLease,
        presented_ip: Option<IpAddr>,
        resolver: &R,
        cancel: &CancellationToken,
    ) -> Result<EgressOutcome<ConsumedConnect>, EgressError> {
        cancel_check(cancel)?;
        let stored = self.take_use(lease.id)?;
        if stored.target != lease.target || stored.bound_ips != lease.bound_ips {
            self.record_audit(deny_audit(
                self.client,
                self.policy.mode,
                &lease.target,
                EgressReason::TargetDrift,
            ))?;
            return Ok(EgressOutcome::Deny(EgressDenial {
                reason: EgressReason::TargetDrift,
                audit: deny_audit(
                    self.client,
                    self.policy.mode,
                    &lease.target,
                    EgressReason::TargetDrift,
                ),
            }));
        }
        let fresh = match normalize_network(&intent_from_target(&stored.target), resolver, cancel) {
            Ok(target) => target,
            Err(NetworkNormalizeError::Cancelled) => return Err(EgressError::Cancelled),
            Err(err) => {
                self.record_error_audit_from_target(&stored.target, false)?;
                return Err(EgressError::Normalize(err));
            }
        };
        cancel_check(cancel)?;
        if !same_origin(&stored.target, &fresh) {
            let denial = deny_outcome(
                self.client,
                self.policy.mode,
                &fresh,
                EgressReason::TargetDrift,
            );
            self.record_audit(denial.audit.clone())?;
            return Ok(EgressOutcome::Deny(denial));
        }
        let fresh_ips = bound_ips(fresh.resolved_ips());
        if !fresh_ips.is_subset(&stored.bound_ips) {
            let denial = deny_outcome(
                self.client,
                self.policy.mode,
                &fresh,
                EgressReason::DnsRebind,
            );
            self.record_audit(denial.audit.clone())?;
            return Ok(EgressOutcome::Deny(denial));
        }
        if let Decision::Deny(reason) =
            decide(&self.policy, &fresh, fresh.redirect().hop() > 0, cancel)?
        {
            let denial = deny_outcome(self.client, self.policy.mode, &fresh, reason);
            self.record_audit(denial.audit.clone())?;
            return Ok(EgressOutcome::Deny(denial));
        }
        let mut dial_ips = fresh_ips;
        if let Some(presented) = presented_ip {
            let presented = canonical_ip(presented);
            if !dial_ips.contains(&presented) {
                let denial = deny_outcome(
                    self.client,
                    self.policy.mode,
                    &fresh,
                    EgressReason::TargetDrift,
                );
                self.record_audit(denial.audit.clone())?;
                return Ok(EgressOutcome::Deny(denial));
            }
            dial_ips = BTreeSet::from([presented]);
        }
        let consumed = ConsumedConnect {
            id: lease.id,
            target: fresh,
            dial_ips,
        };
        self.record_audit(allow_audit(self.client, self.policy.mode, &consumed.target))?;
        Ok(EgressOutcome::Allow(consumed))
    }

    fn issue_lease(&self, target: CanonicalNetworkTarget) -> Result<ConnectLease, EgressError> {
        let bound_ips = bound_ips(target.resolved_ips());
        let id = self.next_lease.fetch_add(1, Ordering::Relaxed);
        let mut leases = self.leases.lock().map_err(|_| EgressError::Unavailable)?;
        leases.insert(
            id,
            LeaseState {
                target: target.clone(),
                bound_ips: bound_ips.clone(),
                remaining: 1,
            },
        );
        Ok(ConnectLease {
            id: ConnectLeaseId(id),
            target,
            bound_ips,
        })
    }

    fn take_use(&self, id: ConnectLeaseId) -> Result<LeaseState, EgressError> {
        let mut leases = self.leases.lock().map_err(|_| EgressError::Unavailable)?;
        let Some(state) = leases.get_mut(&id.0) else {
            return Err(EgressError::UnknownLease);
        };
        if state.remaining == 0 {
            return Err(EgressError::UsesExhausted);
        }
        state.remaining -= 1;
        Ok(LeaseState {
            target: state.target.clone(),
            bound_ips: state.bound_ips.clone(),
            remaining: 0,
        })
    }

    fn record_audit(&self, record: EgressAuditRecord) -> Result<(), EgressError> {
        let mut audit = self.audit.lock().map_err(|_| EgressError::Unavailable)?;
        if audit.len() >= MAX_AUDIT_RECORDS {
            audit.pop_front();
        }
        audit.push_back(record);
        Ok(())
    }

    fn replace_last_audit(&self, record: EgressAuditRecord) -> Result<(), EgressError> {
        let mut audit = self.audit.lock().map_err(|_| EgressError::Unavailable)?;
        if let Some(last) = audit.back_mut() {
            *last = record;
        } else if audit.len() < MAX_AUDIT_RECORDS {
            audit.push_back(record);
        }
        Ok(())
    }

    fn record_error_audit(
        &self,
        intent: &NetworkIntent,
        previous: Option<&CanonicalNetworkTarget>,
    ) -> Result<(), EgressError> {
        let previous_host = previous
            .map(canonical_host_str)
            .or_else(|| intent.previous().map(|hop| hop.host().as_canonical_str()));
        self.record_audit(EgressAuditRecord {
            decision: AuditDecision::Error,
            reason: None,
            client: self.client,
            mode: self.policy.mode,
            scheme: None,
            host: None,
            port: None,
            ip_classes: BTreeSet::new(),
            hop: intent.hop(),
            previous_host,
        })
    }

    fn record_error_audit_from_target(
        &self,
        target: &CanonicalNetworkTarget,
        as_redirect: bool,
    ) -> Result<(), EgressError> {
        self.record_audit(EgressAuditRecord {
            decision: AuditDecision::Error,
            reason: None,
            client: self.client,
            mode: self.policy.mode,
            scheme: Some(target.scheme()),
            host: Some(canonical_host_str(target)),
            port: Some(target.port()),
            ip_classes: target.ip_classes().clone(),
            hop: target
                .redirect()
                .hop()
                .saturating_add(u8::from(as_redirect)),
            previous_host: Some(canonical_host_str(target)),
        })
    }
}

impl ConnectLease {
    pub fn id(&self) -> ConnectLeaseId {
        self.id
    }

    pub fn target(&self) -> &CanonicalNetworkTarget {
        &self.target
    }

    pub fn bound_ips(&self) -> &BTreeSet<IpAddr> {
        &self.bound_ips
    }
}

impl ConnectLeaseId {
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl ConsumedConnect {
    pub fn id(&self) -> ConnectLeaseId {
        self.id
    }

    pub fn target(&self) -> &CanonicalNetworkTarget {
        &self.target
    }

    pub fn dial_ips(&self) -> &BTreeSet<IpAddr> {
        &self.dial_ips
    }
}

impl EgressDenial {
    pub fn reason(&self) -> EgressReason {
        self.reason
    }

    pub fn audit(&self) -> &EgressAuditRecord {
        &self.audit
    }
}

impl EgressReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NetworkDisabled => "network_disabled",
            Self::NotAllowlisted => "not_allowlisted",
            Self::SensitiveClass => "sensitive_class",
            Self::RedirectForbidden => "redirect_forbidden",
            Self::DnsRebind => "dns_rebind",
            Self::TargetDrift => "target_drift",
        }
    }
}

impl EgressAuditRecord {
    pub fn is_allow(&self) -> bool {
        self.decision == AuditDecision::Allow
    }

    pub fn is_deny(&self) -> bool {
        self.decision == AuditDecision::Deny
    }

    pub fn is_error(&self) -> bool {
        self.decision == AuditDecision::Error
    }

    pub fn reason(&self) -> Option<EgressReason> {
        self.reason
    }

    pub fn client(&self) -> NetworkClient {
        self.client
    }

    pub fn mode(&self) -> EgressMode {
        self.mode
    }

    pub fn scheme(&self) -> Option<NetworkScheme> {
        self.scheme
    }

    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn ip_classes(&self) -> &BTreeSet<IpClass> {
        &self.ip_classes
    }

    pub fn hop(&self) -> u8 {
        self.hop
    }

    pub fn previous_host(&self) -> Option<&str> {
        self.previous_host.as_deref()
    }
}

impl EgressOutcome<ConnectLease> {
    pub fn lease(self) -> Result<ConnectLease, EgressDenial> {
        match self {
            Self::Allow(lease) => Ok(lease),
            Self::Deny(deny) => Err(deny),
        }
    }
}

enum Decision {
    Allow,
    Deny(EgressReason),
}

fn decide(
    policy: &EgressPolicy,
    target: &CanonicalNetworkTarget,
    is_redirect: bool,
    cancel: &CancellationToken,
) -> Result<Decision, EgressError> {
    cancel_check(cancel)?;
    if policy.mode == EgressMode::None {
        return Ok(Decision::Deny(if is_redirect {
            EgressReason::RedirectForbidden
        } else {
            EgressReason::NetworkDisabled
        }));
    }
    if !origin_allowlisted(policy, target, cancel)? {
        return Ok(Decision::Deny(if is_redirect {
            EgressReason::RedirectForbidden
        } else {
            EgressReason::NotAllowlisted
        }));
    }
    if has_sensitive_class(target) && !sensitive_explicitly_allowed(policy, target, cancel)? {
        return Ok(Decision::Deny(if is_redirect {
            EgressReason::RedirectForbidden
        } else {
            EgressReason::SensitiveClass
        }));
    }
    Ok(Decision::Allow)
}

fn origin_allowlisted(
    policy: &EgressPolicy,
    target: &CanonicalNetworkTarget,
    cancel: &CancellationToken,
) -> Result<bool, EgressError> {
    for (i, rule) in policy.rules.iter().enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            cancel_check(cancel)?;
        }
        if origin_rule_matches(rule, target) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn origin_rule_matches(rule: &EgressRule, target: &CanonicalNetworkTarget) -> bool {
    if !rule.scheme.is_none_or(|scheme| scheme == target.scheme()) {
        return false;
    }
    if !rule.port.is_none_or(|port| port == target.port()) {
        return false;
    }
    match (&rule.host, target.host()) {
        (EgressHost::Dns(expected), CanonicalNetHost::Dns(got)) => {
            expected.as_str() == got.as_str()
        }
        (EgressHost::Ip(expected), CanonicalNetHost::Ip(got)) => *expected == canonical_ip(*got),
        _ => false,
    }
}

fn has_sensitive_class(target: &CanonicalNetworkTarget) -> bool {
    target.ip_classes().iter().any(|class| {
        matches!(
            class,
            IpClass::Loopback
                | IpClass::Private
                | IpClass::LinkLocal
                | IpClass::MetadataLike
                | IpClass::Unspecified
        )
    })
}

fn sensitive_explicitly_allowed(
    policy: &EgressPolicy,
    target: &CanonicalNetworkTarget,
    cancel: &CancellationToken,
) -> Result<bool, EgressError> {
    if every_ip_has_literal_rule(policy, target, cancel)? {
        return Ok(true);
    }
    inherent_hostname_covers(policy, target, cancel)
}

fn every_ip_has_literal_rule(
    policy: &EgressPolicy,
    target: &CanonicalNetworkTarget,
    cancel: &CancellationToken,
) -> Result<bool, EgressError> {
    if target.resolved_ips().is_empty() {
        return Ok(false);
    }
    for (i, ip) in target.resolved_ips().iter().enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            cancel_check(cancel)?;
        }
        if !literal_ip_rule_covers(policy, target, *ip) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn literal_ip_rule_covers(
    policy: &EgressPolicy,
    target: &CanonicalNetworkTarget,
    ip: IpAddr,
) -> bool {
    let ip = canonical_ip(ip);
    policy.rules.iter().any(|rule| {
        if !rule.scheme.is_none_or(|scheme| scheme == target.scheme()) {
            return false;
        }
        if !rule.port.is_none_or(|port| port == target.port()) {
            return false;
        }
        matches!(&rule.host, EgressHost::Ip(expected) if *expected == ip)
    })
}

fn inherent_hostname_covers(
    policy: &EgressPolicy,
    target: &CanonicalNetworkTarget,
    cancel: &CancellationToken,
) -> Result<bool, EgressError> {
    let CanonicalNetHost::Dns(name) = target.host() else {
        return Ok(false);
    };
    let Some(inherent) = inherent_class(name.as_str()) else {
        return Ok(false);
    };
    if !classes_consistent_with(inherent, target.ip_classes()) {
        return Ok(false);
    }
    origin_allowlisted(policy, target, cancel)
}

fn inherent_class(host: &str) -> Option<IpClass> {
    if INHERENT_LOOPBACK.contains(&host) {
        return Some(IpClass::Loopback);
    }
    if INHERENT_METADATA.contains(&host) {
        return Some(IpClass::MetadataLike);
    }
    None
}

fn classes_consistent_with(inherent: IpClass, classes: &BTreeSet<IpClass>) -> bool {
    match inherent {
        IpClass::Loopback => classes.iter().all(|class| *class == IpClass::Loopback),
        IpClass::MetadataLike => classes
            .iter()
            .all(|class| matches!(class, IpClass::MetadataLike | IpClass::LinkLocal)),
        IpClass::Private => classes.iter().all(|class| *class == IpClass::Private),
        IpClass::LinkLocal => classes.iter().all(|class| *class == IpClass::LinkLocal),
        IpClass::Unspecified => classes.iter().all(|class| *class == IpClass::Unspecified),
        IpClass::Public => classes.iter().all(|class| *class == IpClass::Public),
    }
}

fn bound_ips(ips: &[IpAddr]) -> BTreeSet<IpAddr> {
    ips.iter().copied().map(canonical_ip).collect()
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        IpAddr::V4(_) => ip,
    }
}

fn canonical_host_str(target: &CanonicalNetworkTarget) -> String {
    target.host().as_canonical_str()
}

fn same_origin(left: &CanonicalNetworkTarget, right: &CanonicalNetworkTarget) -> bool {
    left.scheme() == right.scheme()
        && left.port() == right.port()
        && host_identity(left.host()) == host_identity(right.host())
}

fn host_identity(host: &CanonicalNetHost) -> String {
    match host {
        CanonicalNetHost::Dns(name) => name.as_str().to_owned(),
        CanonicalNetHost::Ip(ip) => canonical_ip(*ip).to_string(),
    }
}

fn intent_from_target(target: &CanonicalNetworkTarget) -> NetworkIntent {
    let host = match target.host() {
        CanonicalNetHost::Dns(name) => name.as_str().to_owned(),
        CanonicalNetHost::Ip(IpAddr::V6(v6)) => format!("[{v6}]"),
        CanonicalNetHost::Ip(IpAddr::V4(v4)) => v4.to_string(),
    };
    NetworkIntent::connect(format!(
        "{}://{host}:{}",
        target.scheme().as_str(),
        target.port()
    ))
}

fn allow_audit(
    client: NetworkClient,
    mode: EgressMode,
    target: &CanonicalNetworkTarget,
) -> EgressAuditRecord {
    EgressAuditRecord {
        decision: AuditDecision::Allow,
        reason: None,
        client,
        mode,
        scheme: Some(target.scheme()),
        host: Some(canonical_host_str(target)),
        port: Some(target.port()),
        ip_classes: target.ip_classes().clone(),
        hop: target.redirect().hop(),
        previous_host: target
            .redirect()
            .previous()
            .map(|hop| hop.host().as_canonical_str()),
    }
}

fn deny_audit(
    client: NetworkClient,
    mode: EgressMode,
    target: &CanonicalNetworkTarget,
    reason: EgressReason,
) -> EgressAuditRecord {
    EgressAuditRecord {
        decision: AuditDecision::Deny,
        reason: Some(reason),
        client,
        mode,
        scheme: Some(target.scheme()),
        host: Some(canonical_host_str(target)),
        port: Some(target.port()),
        ip_classes: target.ip_classes().clone(),
        hop: target.redirect().hop(),
        previous_host: target
            .redirect()
            .previous()
            .map(|hop| hop.host().as_canonical_str()),
    }
}

fn deny_outcome(
    client: NetworkClient,
    mode: EgressMode,
    target: &CanonicalNetworkTarget,
    reason: EgressReason,
) -> EgressDenial {
    EgressDenial {
        reason,
        audit: deny_audit(client, mode, target, reason),
    }
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), EgressError> {
    if cancel.is_cancelled() {
        Err(EgressError::Cancelled)
    } else {
        Ok(())
    }
}

impl fmt::Display for NetworkClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EgressMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EgressReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for EgressDenial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "egress denied ({})", self.reason)
    }
}

impl fmt::Display for EgressAuditRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let decision = match self.decision {
            AuditDecision::Allow => "allow",
            AuditDecision::Deny => "deny",
            AuditDecision::Error => "error",
        };
        write!(
            f,
            "egress {decision} client={} mode={}",
            self.client, self.mode
        )?;
        if let Some(reason) = self.reason {
            write!(f, " reason={reason}")?;
        }
        if let (Some(scheme), Some(host), Some(port)) =
            (self.scheme, self.host.as_deref(), self.port)
        {
            write!(f, " target={}:{host}:{port}", scheme.as_str())?;
        }
        Ok(())
    }
}

impl fmt::Display for EgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("egress authorization cancelled"),
            Self::Normalize(err) => write!(f, "egress normalize failed ({err})"),
            Self::Unavailable => f.write_str("egress proxy unavailable"),
            Self::UsesExhausted => f.write_str("connect lease uses exhausted"),
            Self::UnknownLease => f.write_str("unknown connect lease"),
            Self::InvalidRule => f.write_str("invalid egress rule"),
            Self::TooManyRules => f.write_str("egress rule list exceeds bound"),
            Self::InvalidPort => f.write_str("invalid egress port"),
            Self::EmptyHost => f.write_str("egress host is empty"),
        }
    }
}

impl Error for EgressError {}
impl Error for EgressDenial {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct MapResolver {
        records: BTreeMap<String, Vec<IpAddr>>,
    }

    impl MapResolver {
        fn new() -> Self {
            Self {
                records: BTreeMap::new(),
            }
        }

        fn with(mut self, host: &str, ips: &[&str]) -> Self {
            let addrs = ips
                .iter()
                .map(|ip| ip.parse().expect("fixture ip"))
                .collect();
            self.records.insert(host.to_ascii_lowercase(), addrs);
            self
        }
    }

    impl NetworkResolver for MapResolver {
        fn resolve(&self, host: &Hostname) -> Result<Vec<IpAddr>, NetworkNormalizeError> {
            self.records
                .get(host.as_str())
                .cloned()
                .ok_or(NetworkNormalizeError::UnresolvedHost)
        }
    }

    struct FlipResolver {
        first: Vec<IpAddr>,
        next: Vec<IpAddr>,
        calls: AtomicUsize,
    }

    impl FlipResolver {
        fn new(first: &[&str], next: &[&str]) -> Self {
            Self {
                first: first.iter().map(|ip| ip.parse().expect("ip")).collect(),
                next: next.iter().map(|ip| ip.parse().expect("ip")).collect(),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl NetworkResolver for FlipResolver {
        fn resolve(&self, host: &Hostname) -> Result<Vec<IpAddr>, NetworkNormalizeError> {
            if host.as_str() != "example.com" {
                return Err(NetworkNormalizeError::UnresolvedHost);
            }
            let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if n == 0 {
                Ok(self.first.clone())
            } else {
                Ok(self.next.clone())
            }
        }
    }

    fn fixture() -> MapResolver {
        MapResolver::new()
            .with("example.com", &["93.184.216.34"])
            .with("cdn.example.com", &["93.184.216.34"])
            .with("rebind.example.com", &["93.184.216.34", "169.254.169.254"])
            .with("private.internal", &["10.0.0.8"])
            .with("link.internal", &["169.254.1.1"])
            .with("localhost", &["127.0.0.1", "::1"])
            .with("metadata.google.internal", &["169.254.169.254"])
            .with("metadata", &["169.254.169.254"])
    }

    fn allowlist(hosts: &[&str]) -> EgressProxy {
        let rules = hosts
            .iter()
            .map(|host| EgressRule::host(host).expect("rule"))
            .collect::<Vec<_>>();
        EgressProxy::new(
            EgressPolicy::allowlist(rules).expect("policy"),
            NetworkClient::Sandbox,
        )
    }

    fn proxy_mode(hosts: &[&str]) -> EgressProxy {
        let rules = hosts
            .iter()
            .map(|host| EgressRule::host(host).expect("rule"))
            .collect::<Vec<_>>();
        EgressProxy::new(
            EgressPolicy::proxy(rules).expect("policy"),
            NetworkClient::Browser,
        )
    }

    fn allow_connect(proxy: &EgressProxy, url: &str) -> ConnectLease {
        match authorize_connect(
            proxy,
            &NetworkIntent::connect(url),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect("authorize")
        {
            EgressOutcome::Allow(lease) => lease,
            EgressOutcome::Deny(deny) => panic!("denied: {}", deny.reason()),
        }
    }

    fn deny_connect(proxy: &EgressProxy, url: &str) -> EgressDenial {
        match authorize_connect(
            proxy,
            &NetworkIntent::connect(url),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect("authorize")
        {
            EgressOutcome::Allow(_) => panic!("unexpected allow for {url}"),
            EgressOutcome::Deny(deny) => deny,
        }
    }

    #[test]
    fn allowlisted_public_origin_receives_one_use_lease() {
        let proxy = allowlist(&["example.com"]);
        let lease = allow_connect(&proxy, "HTTPS://Example.COM/path");
        assert_eq!(lease.target().scheme(), NetworkScheme::Https);
        assert_eq!(lease.target().port(), 443);
        assert_eq!(lease.target().host().as_canonical_str(), "example.com");
        assert!(
            lease
                .bound_ips()
                .contains(&"93.184.216.34".parse().unwrap())
        );
        let consumed = match proxy
            .consume_connect(&lease, None, &fixture(), &CancellationToken::new())
            .expect("consume")
        {
            EgressOutcome::Allow(consumed) => consumed,
            EgressOutcome::Deny(deny) => panic!("consume denied: {}", deny.reason()),
        };
        assert_eq!(consumed.dial_ips(), lease.bound_ips());
        assert_eq!(
            proxy
                .consume_connect(&lease, None, &fixture(), &CancellationToken::new())
                .expect_err("second use"),
            EgressError::UsesExhausted
        );
        let log = proxy.audit_log().expect("audit");
        assert!(log.iter().any(|row| row.is_allow()));
        assert!(
            !log.iter()
                .any(|row| row.host().is_some_and(|h| h.contains('/')))
        );
    }

    #[test]
    fn unknown_origin_is_denied_not_an_error() {
        let proxy = allowlist(&["example.com"]);
        let deny = deny_connect(&proxy, "https://cdn.example.com");
        assert_eq!(deny.reason(), EgressReason::NotAllowlisted);
        assert!(deny.audit().is_deny());
        assert!(!deny.to_string().contains("cdn.example.com"));
    }

    #[test]
    fn none_mode_disables_network_even_with_rules() {
        let proxy = EgressProxy::new(EgressPolicy::none(), NetworkClient::Tool);
        let deny = deny_connect(&proxy, "https://example.com");
        assert_eq!(deny.reason(), EgressReason::NetworkDisabled);
    }

    #[test]
    fn empty_allowlist_denies() {
        let proxy = EgressProxy::new(
            EgressPolicy::allowlist([]).expect("empty"),
            NetworkClient::Tool,
        );
        assert_eq!(
            deny_connect(&proxy, "https://example.com").reason(),
            EgressReason::NotAllowlisted
        );
    }

    #[test]
    fn loopback_private_link_local_metadata_blocked_without_explicit_allow() {
        let proxy = allowlist(&[
            "example.com",
            "localhost",
            "private.internal",
            "link.internal",
            "metadata.google.internal",
        ]);
        // hostname-only allow of a public name must not cover literals.
        let public_only = allowlist(&["example.com"]);
        assert_eq!(
            deny_connect(&public_only, "http://127.0.0.1").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(
            deny_connect(&public_only, "http://10.1.2.3").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(
            deny_connect(&public_only, "http://169.254.1.1").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(
            deny_connect(&public_only, "http://169.254.169.254/latest/meta-data/").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(
            deny_connect(&public_only, "http://[::1]").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(
            deny_connect(&public_only, "http://[fd00:ec2::254]").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(
            deny_connect(&public_only, "http://0.0.0.0").reason(),
            EgressReason::NotAllowlisted
        );

        // listed public name resolving to private/metadata is still sensitive.
        assert_eq!(
            deny_connect(&proxy, "http://private.internal").reason(),
            EgressReason::SensitiveClass
        );
        assert_eq!(
            deny_connect(&proxy, "http://link.internal").reason(),
            EgressReason::SensitiveClass
        );

        // inherent loopback/metadata hostnames may be granted explicitly.
        let loopback = allow_connect(&proxy, "http://localhost");
        assert!(loopback.target().ip_classes().contains(&IpClass::Loopback));
        let meta = allow_connect(&proxy, "http://metadata.google.internal");
        assert!(meta.target().ip_classes().contains(&IpClass::MetadataLike));
    }

    #[test]
    fn explicit_literal_ip_allows_loopback_and_metadata() {
        let proxy = allowlist(&["127.0.0.1", "169.254.169.254", "::1"]);
        let loopback = allow_connect(&proxy, "http://127.0.0.1:8080");
        assert!(loopback.target().ip_classes().contains(&IpClass::Loopback));
        let mapped = allow_connect(&proxy, "http://[::ffff:127.0.0.1]");
        assert!(mapped.bound_ips().contains(&"127.0.0.1".parse().unwrap()));
        let v6 = allow_connect(&proxy, "http://[::1]");
        assert!(v6.target().ip_classes().contains(&IpClass::Loopback));
        let imds = allow_connect(&proxy, "http://169.254.169.254/");
        assert!(imds.target().ip_classes().contains(&IpClass::MetadataLike));
    }

    #[test]
    fn hostname_allow_does_not_cover_dns_rebind_to_metadata() {
        // T-005: public + IMDS A records fail closed even when the name is listed.
        let proxy = allowlist(&["rebind.example.com", "example.com"]);
        let deny = deny_connect(&proxy, "https://rebind.example.com");
        assert_eq!(deny.reason(), EgressReason::SensitiveClass);
        assert!(deny.audit().ip_classes().contains(&IpClass::MetadataLike));
        assert!(!deny.to_string().contains("169.254"));
        assert!(!deny.audit().to_string().contains("rebind.example.com/"));
    }

    #[test]
    fn hostname_allow_does_not_cover_private_resolution() {
        let proxy = allowlist(&["private.internal"]);
        assert_eq!(
            deny_connect(&proxy, "https://private.internal").reason(),
            EgressReason::SensitiveClass
        );
    }

    #[test]
    fn redirect_to_forbidden_origin_or_ip_is_blocked() {
        let proxy = allowlist(&["example.com"]);
        let first = allow_connect(&proxy, "https://example.com/start");
        let meta = proxy
            .authorize_redirect(
                first.target(),
                "http://169.254.169.254/latest/meta-data/",
                &fixture(),
                &CancellationToken::new(),
            )
            .expect("redirect auth");
        match meta {
            EgressOutcome::Deny(deny) => {
                assert_eq!(deny.reason(), EgressReason::RedirectForbidden);
                assert!(deny.audit().ip_classes().contains(&IpClass::MetadataLike));
                assert_eq!(deny.audit().previous_host(), Some("example.com"));
            }
            EgressOutcome::Allow(_) => panic!("imds redirect allowed"),
        }

        let other = proxy
            .authorize_redirect(
                first.target(),
                "https://cdn.example.com/next",
                &fixture(),
                &CancellationToken::new(),
            )
            .expect("other origin");
        match other {
            EgressOutcome::Deny(deny) => {
                assert_eq!(deny.reason(), EgressReason::RedirectForbidden)
            }
            EgressOutcome::Allow(_) => panic!("unlisted redirect allowed"),
        }

        let same = proxy
            .authorize_redirect(
                first.target(),
                "https://EXAMPLE.COM/next",
                &fixture(),
                &CancellationToken::new(),
            )
            .expect("same origin");
        assert!(matches!(same, EgressOutcome::Allow(_)));
    }

    #[test]
    fn dns_change_between_authorize_and_consume_is_denied() {
        // T-005: lease bound to the public A record cannot follow a rebind.
        let resolver = FlipResolver::new(&["93.184.216.34"], &["169.254.169.254"]);
        let proxy = allowlist(&["example.com"]);
        let lease = match authorize_connect(
            &proxy,
            &NetworkIntent::connect("https://example.com"),
            &resolver,
            &CancellationToken::new(),
        )
        .expect("authorize")
        {
            EgressOutcome::Allow(lease) => lease,
            EgressOutcome::Deny(deny) => panic!("authorize denied: {}", deny.reason()),
        };
        assert!(
            lease
                .bound_ips()
                .contains(&"93.184.216.34".parse().unwrap())
        );
        match proxy
            .consume_connect(&lease, None, &resolver, &CancellationToken::new())
            .expect("consume")
        {
            EgressOutcome::Deny(deny) => assert_eq!(deny.reason(), EgressReason::DnsRebind),
            EgressOutcome::Allow(_) => panic!("rebind consume allowed"),
        }
        assert_eq!(
            proxy
                .consume_connect(&lease, None, &resolver, &CancellationToken::new())
                .expect_err("burned"),
            EgressError::UsesExhausted
        );
    }

    #[test]
    fn presented_ip_outside_bound_set_is_denied() {
        let proxy = allowlist(&["example.com"]);
        let lease = allow_connect(&proxy, "https://example.com");
        match proxy
            .consume_connect(
                &lease,
                Some("169.254.169.254".parse().unwrap()),
                &fixture(),
                &CancellationToken::new(),
            )
            .expect("consume")
        {
            EgressOutcome::Deny(deny) => assert_eq!(deny.reason(), EgressReason::TargetDrift),
            EgressOutcome::Allow(_) => panic!("forged presented ip"),
        }
    }

    #[test]
    fn proxy_mode_enforces_the_same_allow_deny_rules() {
        let proxy = proxy_mode(&["example.com"]);
        assert!(matches!(
            authorize_connect(
                &proxy,
                &NetworkIntent::connect("https://example.com"),
                &fixture(),
                &CancellationToken::new(),
            )
            .expect("allow"),
            EgressOutcome::Allow(_)
        ));
        assert_eq!(
            deny_connect(&proxy, "http://127.0.0.1").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(proxy.client(), NetworkClient::Browser);
        assert_eq!(proxy.policy().mode(), EgressMode::Proxy);
    }

    #[test]
    fn normalize_failure_is_error_not_allow_or_deny() {
        let proxy = allowlist(&["example.com"]);
        let err = authorize_connect(
            &proxy,
            &NetworkIntent::connect("https://user:pass@example.com"),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect_err("userinfo");
        assert!(matches!(
            err,
            EgressError::Normalize(NetworkNormalizeError::Userinfo)
        ));
        let msg = err.to_string();
        assert!(!msg.contains("pass"));
        assert!(!msg.contains("user:"));
        assert!(!msg.contains("example.com"));
        let missing = authorize_connect(
            &proxy,
            &NetworkIntent::connect("https://missing.example.com"),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect_err("missing");
        assert!(matches!(
            missing,
            EgressError::Normalize(NetworkNormalizeError::UnresolvedHost)
        ));
        let log = proxy.audit_log().expect("audit");
        assert!(log.iter().any(|row| row.is_error()));
        assert!(!log.iter().any(|row| row.is_allow()));
    }

    #[test]
    fn cancel_and_rule_bounds_fail_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let proxy = allowlist(&["example.com"]);
        assert_eq!(
            authorize_connect(
                &proxy,
                &NetworkIntent::connect("https://example.com"),
                &fixture(),
                &cancel,
            )
            .expect_err("cancelled"),
            EgressError::Cancelled
        );
        let too_many = (0..=MAX_EGRESS_RULES)
            .map(|i| EgressRule::host(&format!("h{i}.example.com")).expect("rule"))
            .collect::<Vec<_>>();
        assert_eq!(
            EgressPolicy::allowlist(too_many).expect_err("bound"),
            EgressError::TooManyRules
        );
        assert_eq!(
            EgressRule::host("").expect_err("empty"),
            EgressError::EmptyHost
        );
        assert_eq!(
            EgressRule::host_port("example.com", 0).expect_err("port"),
            EgressError::InvalidPort
        );
        assert_eq!(
            EgressRule::host("https://evil.example").expect_err("url-as-host"),
            EgressError::InvalidRule
        );
    }

    #[test]
    fn scheme_and_port_rules_do_not_broaden() {
        let proxy = EgressProxy::new(
            EgressPolicy::allowlist([
                EgressRule::exact(NetworkScheme::Https, "example.com", 443).expect("rule")
            ])
            .expect("policy"),
            NetworkClient::Tool,
        );
        allow_connect(&proxy, "https://example.com");
        assert_eq!(
            deny_connect(&proxy, "http://example.com").reason(),
            EgressReason::NotAllowlisted
        );
        assert_eq!(
            deny_connect(&proxy, "https://example.com:8443").reason(),
            EgressReason::NotAllowlisted
        );
    }
}
