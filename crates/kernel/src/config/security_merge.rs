//! Intersection-only security-policy merge.
//!
//! `base_max` is the higher-trust capability envelope. `lower_scope` may only
//! narrow that envelope. Broadening is a typed error, never a silent union.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use protocol::{NetworkMode, RepoPath};

use super::loader::CancellationToken;

/// Maximum UTF-8 bytes accepted in a network host.
pub const MAX_NETWORK_HOST_BYTES: usize = 253;

/// Maximum UTF-8 bytes accepted in a plugin identifier.
pub const MAX_PLUGIN_ID_BYTES: usize = 128;

/// Maximum hosts accepted on one network policy.
pub const MAX_NETWORK_HOSTS: usize = 256;

/// Maximum filesystem roots accepted on one read or write allowlist.
pub const MAX_FS_ROOTS: usize = 256;

/// Maximum plugin identifiers accepted on one plugin allowlist.
pub const MAX_PLUGIN_IDS: usize = 256;

const CANCEL_CHECK_EVERY: usize = 32;

/// Higher-trust maximum or lower-trust requested security envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityScope {
    network: NetworkPolicy,
    fs: FsPolicy,
    plugins: PluginPolicy,
}

/// Result of [`merge_security`]: the accepted intersection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveSecurityConfig {
    scope: SecurityScope,
}

/// Network grant. `Deny` is strictly tighter than any allow form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkPolicy {
    grant: NetworkGrant,
}

/// Filesystem read/write roots. Write is never implied by read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsPolicy {
    read: BTreeSet<RepoPath>,
    write: BTreeSet<RepoPath>,
}

/// Plugin enablement and optional identifier allowlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginPolicy {
    grant: PluginGrant,
}

/// Exact DNS/IP host after ASCII lowercase normalization.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NetworkHost(String);

/// Plugin identifier. Comparison is exact on the parsed form.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PluginId(String);

/// Field that failed construction or merge.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SecurityField {
    Network,
    FsRead,
    FsWrite,
    Plugin,
}

/// Typed merge or construction failure. Messages never include values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecurityMergeError {
    Cancelled,
    Broadening { field: SecurityField },
    TooManyEntries { field: SecurityField },
    InvalidHost,
    InvalidPlugin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NetworkGrant {
    Deny,
    AllowAny,
    AllowHosts(BTreeSet<NetworkHost>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PluginGrant {
    Disabled,
    AllowAny,
    AllowIds(BTreeSet<PluginId>),
}

impl SecurityScope {
    pub fn new(network: NetworkPolicy, fs: FsPolicy, plugins: PluginPolicy) -> Self {
        Self {
            network,
            fs,
            plugins,
        }
    }

    /// Fail-closed envelope: no network, no fs roots, plugins disabled.
    pub fn deny_all() -> Self {
        Self::new(
            NetworkPolicy::deny(),
            FsPolicy::none(),
            PluginPolicy::disabled(),
        )
    }

    pub fn network(&self) -> &NetworkPolicy {
        &self.network
    }

    pub fn fs(&self) -> &FsPolicy {
        &self.fs
    }

    pub fn plugins(&self) -> &PluginPolicy {
        &self.plugins
    }
}

impl Default for SecurityScope {
    fn default() -> Self {
        Self::deny_all()
    }
}

impl EffectiveSecurityConfig {
    pub fn as_scope(&self) -> &SecurityScope {
        &self.scope
    }

    pub fn into_scope(self) -> SecurityScope {
        self.scope
    }

    pub fn network(&self) -> &NetworkPolicy {
        self.scope.network()
    }

    pub fn fs(&self) -> &FsPolicy {
        self.scope.fs()
    }

    pub fn plugins(&self) -> &PluginPolicy {
        self.scope.plugins()
    }
}

impl NetworkPolicy {
    pub fn deny() -> Self {
        Self {
            grant: NetworkGrant::Deny,
        }
    }

    pub fn allow_any() -> Self {
        Self {
            grant: NetworkGrant::AllowAny,
        }
    }

    pub fn allow_hosts(
        hosts: impl IntoIterator<Item = NetworkHost>,
    ) -> Result<Self, SecurityMergeError> {
        let hosts = collect_bounded(hosts, MAX_NETWORK_HOSTS, SecurityField::Network)?;
        Ok(Self {
            grant: NetworkGrant::AllowHosts(hosts),
        })
    }

    /// Map the documented `sandbox.network` leaf onto a network policy.
    /// Unknown future variants fail closed to deny.
    pub fn from_network_mode(mode: NetworkMode) -> Self {
        if mode == NetworkMode::Allow {
            Self::allow_any()
        } else {
            Self::deny()
        }
    }

    pub fn mode(&self) -> NetworkMode {
        match self.grant {
            NetworkGrant::Deny => NetworkMode::Deny,
            NetworkGrant::AllowAny | NetworkGrant::AllowHosts(_) => NetworkMode::Allow,
        }
    }

    pub fn hosts(&self) -> Option<&BTreeSet<NetworkHost>> {
        match &self.grant {
            NetworkGrant::AllowHosts(hosts) => Some(hosts),
            NetworkGrant::Deny | NetworkGrant::AllowAny => None,
        }
    }

    fn covers(&self, lower: &Self, cancel: &CancellationToken) -> Result<bool, SecurityMergeError> {
        match (&self.grant, &lower.grant) {
            (_, NetworkGrant::Deny) => Ok(true),
            (NetworkGrant::Deny, _) => Ok(false),
            (NetworkGrant::AllowAny, _) => Ok(true),
            (NetworkGrant::AllowHosts(_), NetworkGrant::AllowAny) => Ok(false),
            (NetworkGrant::AllowHosts(base), NetworkGrant::AllowHosts(lower)) => {
                Ok(is_subset_cancelled(base, lower, cancel)?)
            }
        }
    }
}

impl FsPolicy {
    pub fn none() -> Self {
        Self {
            read: BTreeSet::new(),
            write: BTreeSet::new(),
        }
    }

    pub fn new(
        read: impl IntoIterator<Item = RepoPath>,
        write: impl IntoIterator<Item = RepoPath>,
    ) -> Result<Self, SecurityMergeError> {
        Ok(Self {
            read: collect_bounded(read, MAX_FS_ROOTS, SecurityField::FsRead)?,
            write: collect_bounded(write, MAX_FS_ROOTS, SecurityField::FsWrite)?,
        })
    }

    pub fn read(&self) -> &BTreeSet<RepoPath> {
        &self.read
    }

    pub fn write(&self) -> &BTreeSet<RepoPath> {
        &self.write
    }

    fn read_covers(
        &self,
        lower: &Self,
        cancel: &CancellationToken,
    ) -> Result<bool, SecurityMergeError> {
        roots_covered(&self.read, &lower.read, cancel)
    }

    fn write_covers(
        &self,
        lower: &Self,
        cancel: &CancellationToken,
    ) -> Result<bool, SecurityMergeError> {
        roots_covered(&self.write, &lower.write, cancel)
    }
}

impl PluginPolicy {
    pub fn disabled() -> Self {
        Self {
            grant: PluginGrant::Disabled,
        }
    }

    pub fn allow_any() -> Self {
        Self {
            grant: PluginGrant::AllowAny,
        }
    }

    pub fn allow_only(ids: impl IntoIterator<Item = PluginId>) -> Result<Self, SecurityMergeError> {
        let ids = collect_bounded(ids, MAX_PLUGIN_IDS, SecurityField::Plugin)?;
        Ok(Self {
            grant: PluginGrant::AllowIds(ids),
        })
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self.grant, PluginGrant::Disabled)
    }

    pub fn allowed(&self) -> Option<&BTreeSet<PluginId>> {
        match &self.grant {
            PluginGrant::AllowIds(ids) => Some(ids),
            PluginGrant::Disabled | PluginGrant::AllowAny => None,
        }
    }

    fn covers(&self, lower: &Self, cancel: &CancellationToken) -> Result<bool, SecurityMergeError> {
        match (&self.grant, &lower.grant) {
            (_, PluginGrant::Disabled) => Ok(true),
            (PluginGrant::Disabled, _) => Ok(false),
            (PluginGrant::AllowAny, _) => Ok(true),
            (PluginGrant::AllowIds(_), PluginGrant::AllowAny) => Ok(false),
            (PluginGrant::AllowIds(base), PluginGrant::AllowIds(lower)) => {
                Ok(is_subset_cancelled(base, lower, cancel)?)
            }
        }
    }
}

impl NetworkHost {
    pub fn parse(raw: &str) -> Result<Self, SecurityMergeError> {
        if raw.is_empty() {
            return Err(SecurityMergeError::InvalidHost);
        }
        if raw.len() > MAX_NETWORK_HOST_BYTES {
            return Err(SecurityMergeError::InvalidHost);
        }
        if !raw.is_ascii() || raw.chars().any(|c| c.is_ascii_control()) {
            return Err(SecurityMergeError::InvalidHost);
        }
        let lowered = raw.to_ascii_lowercase();
        if looks_like_url_or_userinfo(&lowered) {
            return Err(SecurityMergeError::InvalidHost);
        }
        if is_bracketed_ipv6(&lowered) {
            return Ok(Self(lowered));
        }
        if !is_dns_or_ipv4(&lowered) {
            return Err(SecurityMergeError::InvalidHost);
        }
        Ok(Self(lowered))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PluginId {
    pub fn parse(raw: &str) -> Result<Self, SecurityMergeError> {
        if raw.is_empty() || raw.len() > MAX_PLUGIN_ID_BYTES {
            return Err(SecurityMergeError::InvalidPlugin);
        }
        if raw == "." || raw == ".." {
            return Err(SecurityMergeError::InvalidPlugin);
        }
        let mut chars = raw.chars();
        let Some(first) = chars.next() else {
            return Err(SecurityMergeError::InvalidPlugin);
        };
        if !first.is_ascii_alphanumeric() {
            return Err(SecurityMergeError::InvalidPlugin);
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(SecurityMergeError::InvalidPlugin);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SecurityField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::Plugin => "plugin",
        }
    }
}

impl fmt::Display for SecurityField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for SecurityMergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("security config merge cancelled"),
            Self::Broadening { field } => {
                write!(f, "lower-trust security config broadens {field}")
            }
            Self::TooManyEntries { field } => write!(f, "too many {field} entries"),
            Self::InvalidHost => f.write_str("invalid network host"),
            Self::InvalidPlugin => f.write_str("invalid plugin identifier"),
        }
    }
}

impl Error for SecurityMergeError {}

/// Intersect `base_max` with `lower_scope`. Lower-trust extra grants fail closed.
pub fn merge_security(
    base_max: &SecurityScope,
    lower_scope: &SecurityScope,
    cancel: &CancellationToken,
) -> Result<EffectiveSecurityConfig, SecurityMergeError> {
    cancel.check().map_err(|_| SecurityMergeError::Cancelled)?;

    if !base_max.network.covers(&lower_scope.network, cancel)? {
        return Err(SecurityMergeError::Broadening {
            field: SecurityField::Network,
        });
    }
    cancel.check().map_err(|_| SecurityMergeError::Cancelled)?;

    if !base_max.fs.read_covers(&lower_scope.fs, cancel)? {
        return Err(SecurityMergeError::Broadening {
            field: SecurityField::FsRead,
        });
    }
    cancel.check().map_err(|_| SecurityMergeError::Cancelled)?;

    if !base_max.fs.write_covers(&lower_scope.fs, cancel)? {
        return Err(SecurityMergeError::Broadening {
            field: SecurityField::FsWrite,
        });
    }
    cancel.check().map_err(|_| SecurityMergeError::Cancelled)?;

    if !base_max.plugins.covers(&lower_scope.plugins, cancel)? {
        return Err(SecurityMergeError::Broadening {
            field: SecurityField::Plugin,
        });
    }
    cancel.check().map_err(|_| SecurityMergeError::Cancelled)?;

    // lower ⊆ base, so the accepted intersection is the lower-trust request.
    Ok(EffectiveSecurityConfig {
        scope: lower_scope.clone(),
    })
}

fn collect_bounded<T: Ord>(
    items: impl IntoIterator<Item = T>,
    max: usize,
    field: SecurityField,
) -> Result<BTreeSet<T>, SecurityMergeError> {
    let mut set = BTreeSet::new();
    for item in items {
        if set.len() >= max && !set.contains(&item) {
            return Err(SecurityMergeError::TooManyEntries { field });
        }
        set.insert(item);
    }
    Ok(set)
}

fn is_subset_cancelled<T: Ord>(
    base: &BTreeSet<T>,
    lower: &BTreeSet<T>,
    cancel: &CancellationToken,
) -> Result<bool, SecurityMergeError> {
    for (i, item) in lower.iter().enumerate() {
        if i % CANCEL_CHECK_EVERY == 0 {
            cancel.check().map_err(|_| SecurityMergeError::Cancelled)?;
        }
        if !base.contains(item) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn roots_covered(
    base: &BTreeSet<RepoPath>,
    lower: &BTreeSet<RepoPath>,
    cancel: &CancellationToken,
) -> Result<bool, SecurityMergeError> {
    for (i, requested) in lower.iter().enumerate() {
        if i % CANCEL_CHECK_EVERY == 0 {
            cancel.check().map_err(|_| SecurityMergeError::Cancelled)?;
        }
        if !base
            .iter()
            .any(|allowed| path_is_within(allowed, requested))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn path_is_within(root: &RepoPath, path: &RepoPath) -> bool {
    let root: Vec<&str> = root.components().collect();
    let path: Vec<&str> = path.components().collect();
    path.starts_with(&root)
}

fn looks_like_url_or_userinfo(host: &str) -> bool {
    host.contains("://")
        || host.contains('/')
        || host.contains('\\')
        || host.contains('@')
        || host.contains(' ')
        || host.contains('*')
        || host.starts_with('.')
        || host.ends_with('.')
}

fn is_bracketed_ipv6(host: &str) -> bool {
    let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) else {
        return false;
    };
    if inner.is_empty() || inner.contains('[') || inner.contains(']') {
        return false;
    }
    inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':') && inner.contains(':')
}

fn is_dns_or_ipv4(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_NETWORK_HOST_BYTES {
        return false;
    }
    host.split('.').all(is_dns_label)
}

fn is_dns_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let bytes = label.as_bytes();
    bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::RepoPathError;

    const SECRET: &str = "super-secret-password";

    fn host(raw: &str) -> NetworkHost {
        NetworkHost::parse(raw).unwrap_or_else(|err| panic!("host {raw:?}: {err}"))
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).unwrap_or_else(|err| panic!("path {raw:?}: {err}"))
    }

    fn plugin(raw: &str) -> PluginId {
        PluginId::parse(raw).unwrap_or_else(|err| panic!("plugin {raw:?}: {err}"))
    }

    fn merge(
        base: &SecurityScope,
        lower: &SecurityScope,
    ) -> Result<EffectiveSecurityConfig, SecurityMergeError> {
        merge_security(base, lower, &CancellationToken::new())
    }

    fn org_max() -> SecurityScope {
        SecurityScope::new(
            NetworkPolicy::allow_hosts([host("api.example.com"), host("cdn.example.com")]).unwrap(),
            FsPolicy::new([path("src"), path("docs")], [path("src"), path("tmp/out")]).unwrap(),
            PluginPolicy::allow_only([plugin("fmt"), plugin("lint")]).unwrap(),
        )
    }

    #[test]
    fn identical_scope_is_accepted() {
        let base = org_max();
        let effective = merge(&base, &base).expect("identical");
        assert_eq!(effective.as_scope(), &base);
    }

    #[test]
    fn more_restrictive_workspace_network_is_accepted() {
        let base = org_max();
        let lower = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src/crate")], [path("src/crate")]).unwrap(),
            PluginPolicy::allow_only([plugin("fmt")]).unwrap(),
        );
        let effective = merge(&base, &lower).expect("restrictive");
        assert_eq!(effective.network().mode(), NetworkMode::Deny);
        assert_eq!(
            effective
                .fs()
                .read()
                .iter()
                .map(RepoPath::as_str)
                .collect::<Vec<_>>(),
            vec!["src/crate"]
        );
        assert_eq!(
            effective
                .plugins()
                .allowed()
                .expect("allowlist")
                .iter()
                .map(PluginId::as_str)
                .collect::<Vec<_>>(),
            vec!["fmt"]
        );
    }

    #[test]
    fn workspace_may_disable_plugins_and_clear_fs_write() {
        let base = org_max();
        let lower = SecurityScope::new(
            NetworkPolicy::allow_hosts([host("API.EXAMPLE.COM")]).unwrap(),
            FsPolicy::new([path("src")], []).unwrap(),
            PluginPolicy::disabled(),
        );
        let effective = merge(&base, &lower).expect("tighter");
        assert_eq!(
            effective
                .network()
                .hosts()
                .expect("hosts")
                .iter()
                .map(NetworkHost::as_str)
                .collect::<Vec<_>>(),
            vec!["api.example.com"]
        );
        assert!(effective.fs().write().is_empty());
        assert!(!effective.plugins().is_enabled());
    }

    #[test]
    fn network_allow_from_workspace_cannot_override_deny() {
        let base = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::none(),
            PluginPolicy::disabled(),
        );
        let lower = SecurityScope::new(
            NetworkPolicy::allow_any(),
            FsPolicy::none(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &lower),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::Network,
            })
        );
    }

    #[test]
    fn workspace_cannot_add_network_host() {
        let base = org_max();
        let lower = SecurityScope::new(
            NetworkPolicy::allow_hosts([host("api.example.com"), host("evil.example.com")])
                .unwrap(),
            FsPolicy::new([path("src")], []).unwrap(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &lower),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::Network,
            })
        );
    }

    #[test]
    fn workspace_cannot_promote_restricted_network_to_allow_any() {
        let base = org_max();
        let lower = SecurityScope::new(
            NetworkPolicy::allow_any(),
            FsPolicy::none(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &lower),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::Network,
            })
        );
    }

    #[test]
    fn workspace_cannot_add_fs_read_or_write_root() {
        let base = org_max();
        let extra_read = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src"), path("secrets")], []).unwrap(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &extra_read),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::FsRead,
            })
        );

        let extra_write = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src")], [path("src"), path("secrets")]).unwrap(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &extra_write),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::FsWrite,
            })
        );
    }

    #[test]
    fn workspace_cannot_escalate_read_root_to_write() {
        let base = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src")], []).unwrap(),
            PluginPolicy::disabled(),
        );
        let lower = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src")], [path("src")]).unwrap(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &lower),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::FsWrite,
            })
        );
    }

    #[test]
    fn sibling_path_is_not_covered_by_string_prefix() {
        let base = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src")], [path("src")]).unwrap(),
            PluginPolicy::disabled(),
        );
        let lower = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src2")], [path("src2")]).unwrap(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &lower),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::FsRead,
            })
        );
    }

    #[test]
    fn parent_of_allowed_root_is_broadening() {
        let base = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src/crate")], [path("src/crate")]).unwrap(),
            PluginPolicy::disabled(),
        );
        let lower = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::new([path("src")], [path("src")]).unwrap(),
            PluginPolicy::disabled(),
        );
        assert_eq!(
            merge(&base, &lower),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::FsRead,
            })
        );
    }

    #[test]
    fn workspace_cannot_enable_or_add_plugins() {
        let base = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::none(),
            PluginPolicy::disabled(),
        );
        let enable = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::none(),
            PluginPolicy::allow_only([plugin("fmt")]).unwrap(),
        );
        assert_eq!(
            merge(&base, &enable),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::Plugin,
            })
        );

        let base = org_max();
        let extra = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::none(),
            PluginPolicy::allow_only([plugin("fmt"), plugin("native-shell")]).unwrap(),
        );
        assert_eq!(
            merge(&base, &extra),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::Plugin,
            })
        );

        let any = SecurityScope::new(
            NetworkPolicy::deny(),
            FsPolicy::none(),
            PluginPolicy::allow_any(),
        );
        assert_eq!(
            merge(&base, &any),
            Err(SecurityMergeError::Broadening {
                field: SecurityField::Plugin,
            })
        );
    }

    #[test]
    fn traversal_and_absolute_fs_paths_are_rejected_before_merge() {
        assert_eq!(RepoPath::parse("../secrets"), Err(RepoPathError::Traversal));
        assert_eq!(RepoPath::parse("/etc/passwd"), Err(RepoPathError::Absolute));
        assert_eq!(
            RepoPath::parse("src/../../etc/passwd"),
            Err(RepoPathError::Traversal)
        );
    }

    #[test]
    fn host_and_plugin_construction_rejects_bypass_forms() {
        for raw in [
            "",
            "exa mple.com",
            "user:pass@host",
            "https://evil.example",
            "*.example.com",
            ".example.com",
            "example.com.",
            "host/path",
            "host:443",
            "p@ss/word",
        ] {
            assert_eq!(
                NetworkHost::parse(raw),
                Err(SecurityMergeError::InvalidHost),
                "{raw}"
            );
        }
        for raw in [
            "",
            ".",
            "..",
            "../evil",
            "plug/in",
            "has space",
            "p@ss/word",
        ] {
            assert_eq!(
                PluginId::parse(raw),
                Err(SecurityMergeError::InvalidPlugin),
                "{raw}"
            );
        }
        let host_err = NetworkHost::parse(&format!("user:{SECRET}@host")).expect_err("host");
        let plugin_err = PluginId::parse(&format!("plug/{SECRET}")).expect_err("plugin");
        let rendered = format!("{host_err}{host_err:?}{plugin_err}{plugin_err:?}");
        assert!(!rendered.contains(SECRET));
    }

    #[test]
    fn too_many_entries_fail_closed() {
        let hosts = (0..=MAX_NETWORK_HOSTS).map(|i| host(&format!("h{i}.example.com")));
        assert_eq!(
            NetworkPolicy::allow_hosts(hosts),
            Err(SecurityMergeError::TooManyEntries {
                field: SecurityField::Network,
            })
        );
    }

    #[test]
    fn cancelled_merge_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err =
            merge_security(&org_max(), &SecurityScope::deny_all(), &cancel).expect_err("cancelled");
        assert_eq!(err, SecurityMergeError::Cancelled);
    }

    #[test]
    fn errors_name_field_without_secret_values() {
        let err = SecurityMergeError::Broadening {
            field: SecurityField::Plugin,
        };
        let rendered = format!("{err}{err:?}");
        assert!(rendered.contains("plugin"));
        assert!(!rendered.contains(SECRET));
        assert_eq!(
            SecurityMergeError::InvalidHost.to_string(),
            "invalid network host"
        );
    }

    #[test]
    fn sandbox_network_mode_maps_to_policy() {
        assert_eq!(
            NetworkPolicy::from_network_mode(NetworkMode::Deny),
            NetworkPolicy::deny()
        );
        assert_eq!(
            NetworkPolicy::from_network_mode(NetworkMode::Allow),
            NetworkPolicy::allow_any()
        );
    }

    #[test]
    fn three_layer_merge_stays_intersection() {
        let org = SecurityScope::new(
            NetworkPolicy::allow_any(),
            FsPolicy::new([path("src")], [path("src")]).unwrap(),
            PluginPolicy::allow_any(),
        );
        let user = merge(
            &org,
            &SecurityScope::new(
                NetworkPolicy::allow_hosts([host("api.example.com"), host("cdn.example.com")])
                    .unwrap(),
                FsPolicy::new([path("src")], [path("src/out")]).unwrap(),
                PluginPolicy::allow_only([plugin("fmt"), plugin("lint")]).unwrap(),
            ),
        )
        .expect("user")
        .into_scope();
        let workspace = merge(
            &user,
            &SecurityScope::new(
                NetworkPolicy::allow_hosts([host("api.example.com")]).unwrap(),
                FsPolicy::new([path("src/crate")], []).unwrap(),
                PluginPolicy::allow_only([plugin("fmt")]).unwrap(),
            ),
        )
        .expect("workspace");
        assert_eq!(
            workspace
                .network()
                .hosts()
                .expect("hosts")
                .iter()
                .map(NetworkHost::as_str)
                .collect::<Vec<_>>(),
            vec!["api.example.com"]
        );
        assert!(workspace.fs().write().is_empty());
        assert_eq!(
            workspace
                .plugins()
                .allowed()
                .expect("ids")
                .iter()
                .map(PluginId::as_str)
                .collect::<Vec<_>>(),
            vec!["fmt"]
        );
    }
}
