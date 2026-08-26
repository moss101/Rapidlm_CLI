//! Atomic plugin install/remove transaction.
//!
//! Stage the package, validate manifest/hash/signature/policy, then publish
//! with rename or roll back. Validation never instantiates or executes the
//! guest (T-006). Source URL and package metadata are untrusted data, never
//! authority. An interrupted install is not an active plugin. Remove revokes
//! executable status and keeps the trust audit record.

use std::error::Error;
use std::fmt::{self, Debug};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use capability_broker::CancellationToken;
use protocol::{ApiError, ArtifactId, ErrorCode, TraceId};

use crate::manifest::{
    parse_manifest, validate_for_install, ManifestError, PluginId, PluginManifest,
};
use crate::trust::{
    ExtensionIdentity, ExtensionObservation, ExtensionTrustRecord, ExtensionTrustStore,
    InstallSource, ReviewTimestamp, TrustError, TrustInspectView, TrustScope, TrustedCapability,
};

/// Maximum accepted package payload in bytes (matches WASM hard module cap).
pub const MAX_PACKAGE_BYTES: usize = 4 * 1024 * 1024;

/// Directory of published, active plugins.
pub const INSTALLED_DIR: &str = "installed";

/// In-progress transaction directory. Never treated as active.
pub const STAGING_DIR: &str = "staging";

/// Previous generation kept only for crash restore. Never treated as active.
pub const ROLLBACK_DIR: &str = "rollback";

/// Trust catalog file name under the installer root.
pub const TRUST_CATALOG_FILE: &str = "plugin-trust.json";

const MANIFEST_FILE: &str = "manifest.json";
const PACKAGE_FILE: &str = "package.bin";
const DISABLED_FILE: &str = "disabled.marker";
const WRITE_CHUNK: usize = 64 * 1024;
const CANCEL_STRIDE: usize = 16;

static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Durable plugin install/remove store.
pub struct PluginInstaller {
    root: PathBuf,
    installed: PathBuf,
    staging: PathBuf,
    rollback: PathBuf,
    trust: ExtensionTrustStore,
    max_package_bytes: usize,
}

/// Caller-supplied install input. Source/locator/metadata are untrusted data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallRequest {
    manifest: Vec<u8>,
    package: Vec<u8>,
    signature: Option<ArtifactId>,
    source: InstallSource,
    scope: TrustScope,
    last_review: ReviewTimestamp,
    policy: InstallPolicy,
}

/// Manifest/hash/signature/capability pins. Pins never grant capability.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InstallPolicy {
    expected_hash: Option<ArtifactId>,
    expected_signature: Option<ArtifactId>,
    require_signature: bool,
    allowlist: Option<Vec<TrustedCapability>>,
}

/// Successful publish. `executable_enabled` is always false after install.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallReceipt {
    identity: ExtensionIdentity,
    source: InstallSource,
    scope: TrustScope,
    package_hash: ArtifactId,
    path: PathBuf,
    executable_enabled: bool,
}

/// Successful remove. The trust audit record remains inspectable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoveReceipt {
    identity: ExtensionIdentity,
    audit: TrustInspectView,
}

/// Successful enable/disable transition. Trust records are unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetEnabledReceipt {
    identity: ExtensionIdentity,
    enabled: bool,
}

impl SetEnabledReceipt {
    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// Verified on-disk install. Presence here is the only active-plugin signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledPlugin {
    identity: ExtensionIdentity,
    manifest: PluginManifest,
    path: PathBuf,
}

/// Typed install/remove failure. Display never echoes source/locator/bytes.
#[derive(Debug)]
pub enum InstallError {
    Cancelled,
    PackageTooLarge { limit: usize, observed: usize },
    HashMismatch,
    Quarantined,
    PolicyDenied,
    NotInstalled,
    UnsafePath,
    Manifest(ManifestError),
    Trust(TrustError),
    Io(io::Error),
}

impl InstallRequest {
    pub fn new(
        manifest: impl Into<Vec<u8>>,
        package: impl Into<Vec<u8>>,
        source: InstallSource,
        scope: TrustScope,
        last_review: ReviewTimestamp,
    ) -> Self {
        Self {
            manifest: manifest.into(),
            package: package.into(),
            signature: None,
            source,
            scope,
            last_review,
            policy: InstallPolicy::default(),
        }
    }

    pub fn with_signature(mut self, signature: ArtifactId) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn with_policy(mut self, policy: InstallPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    pub fn package(&self) -> &[u8] {
        &self.package
    }

    pub fn signature(&self) -> Option<ArtifactId> {
        self.signature
    }

    pub fn source(&self) -> &InstallSource {
        &self.source
    }

    pub fn scope(&self) -> TrustScope {
        self.scope
    }

    pub fn last_review(&self) -> &ReviewTimestamp {
        &self.last_review
    }

    pub fn policy(&self) -> &InstallPolicy {
        &self.policy
    }
}

impl InstallPolicy {
    /// Closed-taxonomy / ambient checks only. No extra pin or allowlist.
    pub fn manifest_only() -> Self {
        Self::default()
    }

    pub fn with_expected_hash(mut self, hash: ArtifactId) -> Self {
        self.expected_hash = Some(hash);
        self
    }

    pub fn with_expected_signature(mut self, signature: ArtifactId) -> Self {
        self.expected_signature = Some(signature);
        self
    }

    pub fn require_signature(mut self) -> Self {
        self.require_signature = true;
        self
    }

    /// Narrowing allowlist. Requested caps outside it fail closed.
    pub fn with_allowlist<I>(mut self, caps: I) -> Result<Self, InstallError>
    where
        I: IntoIterator<Item = TrustedCapability>,
    {
        let mut allowlist = Vec::new();
        for cap in caps {
            allowlist.push(cap);
        }
        self.allowlist = Some(allowlist);
        Ok(self)
    }

    pub fn expected_hash(&self) -> Option<ArtifactId> {
        self.expected_hash
    }

    pub fn expected_signature(&self) -> Option<ArtifactId> {
        self.expected_signature
    }

    pub fn signature_required(&self) -> bool {
        self.require_signature || self.expected_signature.is_some()
    }

    pub fn allowlist(&self) -> Option<&[TrustedCapability]> {
        self.allowlist.as_deref()
    }
}

impl InstallReceipt {
    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }

    pub fn source(&self) -> &InstallSource {
        &self.source
    }

    pub fn scope(&self) -> TrustScope {
        self.scope
    }

    pub fn package_hash(&self) -> ArtifactId {
        self.package_hash
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Install never enables executable code. A later explicit grant is required.
    pub fn executable_enabled(&self) -> bool {
        self.executable_enabled
    }
}

impl RemoveReceipt {
    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }

    pub fn audit(&self) -> &TrustInspectView {
        &self.audit
    }
}

impl InstalledPlugin {
    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PluginInstaller {
    /// Open an installer rooted at `root`. Leftover staging is discarded.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, InstallError> {
        let root = root.into();
        let installed = root.join(INSTALLED_DIR);
        let staging = root.join(STAGING_DIR);
        let rollback = root.join(ROLLBACK_DIR);
        let trust = ExtensionTrustStore::open(root.join(TRUST_CATALOG_FILE));
        let installer = Self {
            root,
            installed,
            staging,
            rollback,
            trust,
            max_package_bytes: MAX_PACKAGE_BYTES,
        };
        installer.ensure_layout()?;
        installer.recover(&CancellationToken::new())?;
        Ok(installer)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn trust(&self) -> &ExtensionTrustStore {
        &self.trust
    }

    /// Stage, validate, then atomically publish. Failure leaves no new active plugin.
    ///
    /// The package is never instantiated. Source locators are stored as data.
    pub fn install(
        &self,
        request: &InstallRequest,
        cancel: &CancellationToken,
    ) -> Result<InstallReceipt, InstallError> {
        cancel_check(cancel)?;
        self.recover(cancel)?;
        let prepared = self.validate_request(request, cancel)?;
        let staging = match self.stage_package(&prepared, cancel) {
            Ok(path) => path,
            Err(err) => {
                self.purge_staging();
                return Err(err);
            }
        };
        let dest = self.installed_dir(prepared.identity.plugin());
        let outcome = self.publish(&staging, &dest, prepared.identity.plugin(), cancel);
        if let Err(err) = outcome {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }
        let observation = ExtensionObservation::new(
            prepared.identity.clone(),
            request.source.clone(),
            request.scope,
            request.last_review.clone(),
        );
        match self.trust.register(&observation, cancel) {
            Ok(record) => Ok(receipt_from(&prepared, &dest, &record)),
            Err(err) => {
                let _ = self.restore_previous(prepared.identity.plugin());
                let _ = fs::remove_dir_all(&dest);
                Err(InstallError::Trust(err))
            }
        }
    }

    /// Deactivate and delete the published tree. The trust audit record remains.
    pub fn remove(
        &self,
        plugin: &str,
        last_review: ReviewTimestamp,
        cancel: &CancellationToken,
    ) -> Result<RemoveReceipt, InstallError> {
        cancel_check(cancel)?;
        self.recover(cancel)?;
        let plugin = PluginId::parse(plugin).map_err(InstallError::Manifest)?;
        let installed = self
            .load_installed(&plugin, cancel)?
            .ok_or(InstallError::NotInstalled)?;
        let revoked = self
            .trust
            .revoke(&installed.identity, last_review, cancel)
            .map_err(InstallError::Trust)?;
        self.delete_published(&plugin)?;
        Ok(RemoveReceipt {
            identity: installed.identity,
            audit: revoked.inspect_view(),
        })
    }

    /// Atomically replace an already-installed plugin with a new validated
    /// package. Fails closed with [`InstallError::NotInstalled`] when the
    /// plugin is absent; a disabled plugin stays disabled across update.
    pub fn update(
        &self,
        request: &InstallRequest,
        cancel: &CancellationToken,
    ) -> Result<InstallReceipt, InstallError> {
        cancel_check(cancel)?;
        self.recover(cancel)?;
        let prepared = self.validate_request(request, cancel)?;
        let plugin = prepared.identity.plugin().clone();
        if self.load_installed(&plugin, cancel)?.is_none() {
            return Err(InstallError::NotInstalled);
        }
        // Republish replaces the tree, so preserve an explicit disable state.
        let was_disabled = self.installed_dir(&plugin).join(DISABLED_FILE).exists();
        let receipt = self.install(request, cancel)?;
        if was_disabled {
            let marker = self
                .installed_dir(receipt.identity().plugin())
                .join(DISABLED_FILE);
            fs::write(marker, b"disabled\n").map_err(InstallError::Io)?;
        }
        Ok(receipt)
    }

    /// Deactivate without deleting: durable marker inside the published tree.
    /// Idempotent; trust records are unchanged; the tree stays inspectable.
    pub fn disable(
        &self,
        plugin: &str,
        cancel: &CancellationToken,
    ) -> Result<SetEnabledReceipt, InstallError> {
        self.set_enabled(plugin, false, cancel)
    }

    /// Re-enable a previously disabled plugin. Idempotent.
    pub fn enable(
        &self,
        plugin: &str,
        cancel: &CancellationToken,
    ) -> Result<SetEnabledReceipt, InstallError> {
        self.set_enabled(plugin, true, cancel)
    }

    fn set_enabled(
        &self,
        plugin: &str,
        enabled: bool,
        cancel: &CancellationToken,
    ) -> Result<SetEnabledReceipt, InstallError> {
        cancel_check(cancel)?;
        self.recover(cancel)?;
        let plugin = PluginId::parse(plugin).map_err(InstallError::Manifest)?;
        let installed = self
            .load_installed(&plugin, cancel)?
            .ok_or(InstallError::NotInstalled)?;
        let marker = self.installed_dir(&plugin).join(DISABLED_FILE);
        if enabled {
            match fs::remove_file(&marker) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(InstallError::Io(err)),
            }
        } else {
            fs::write(&marker, b"disabled\n").map_err(InstallError::Io)?;
        }
        Ok(SetEnabledReceipt {
            identity: installed.identity,
            enabled,
        })
    }

    /// True only when a fully published tree verifies AND no disable marker
    /// exists. Staging/rollback are ignored.
    pub fn is_active(
        &self,
        plugin: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, InstallError> {
        cancel_check(cancel)?;
        let plugin = PluginId::parse(plugin).map_err(InstallError::Manifest)?;
        let present = self.load_installed(&plugin, cancel)?.is_some();
        let disabled = self.installed_dir(&plugin).join(DISABLED_FILE).exists();
        Ok(present && !disabled)
    }

    pub fn get(
        &self,
        plugin: &str,
        cancel: &CancellationToken,
    ) -> Result<InstalledPlugin, InstallError> {
        cancel_check(cancel)?;
        let plugin = PluginId::parse(plugin).map_err(InstallError::Manifest)?;
        self.load_installed(&plugin, cancel)?
            .ok_or(InstallError::NotInstalled)
    }

    /// Discard leftover staging and restore a previous generation if needed.
    pub fn recover(&self, cancel: &CancellationToken) -> Result<(), InstallError> {
        cancel_check(cancel)?;
        self.ensure_layout()?;
        self.purge_staging();
        self.restore_orphaned_rollbacks(cancel)?;
        Ok(())
    }

    fn validate_request(
        &self,
        request: &InstallRequest,
        cancel: &CancellationToken,
    ) -> Result<PreparedInstall, InstallError> {
        cancel_check(cancel)?;
        if request.package.len() > self.max_package_bytes {
            return Err(InstallError::PackageTooLarge {
                limit: self.max_package_bytes,
                observed: request.package.len(),
            });
        }
        // Hash and parse only. The guest module is never linked or executed.
        let package_hash = ArtifactId::from_bytes(&request.package);
        let manifest = parse_manifest(&request.manifest, cancel).map_err(map_manifest)?;
        validate_for_install(&manifest).map_err(map_manifest)?;
        if manifest.digest() != package_hash {
            return Err(InstallError::HashMismatch);
        }
        if let Some(expected) = request.policy.expected_hash {
            if expected != package_hash {
                return Err(InstallError::HashMismatch);
            }
        }
        if request.policy.signature_required() && request.signature.is_none() {
            self.record_quarantine(&manifest, request, package_hash, cancel)?;
            return Err(InstallError::Quarantined);
        }
        if let Some(expected) = request.policy.expected_signature {
            if request.signature != Some(expected) {
                self.record_quarantine(&manifest, request, package_hash, cancel)?;
                return Err(InstallError::Quarantined);
            }
        }
        check_policy(&manifest, &request.policy)?;
        let mut identity = ExtensionIdentity::from_binding(&manifest.trust_binding());
        if let Some(signature) = request.signature {
            identity = identity
                .with_signature(&signature.to_string())
                .map_err(InstallError::Trust)?;
        }
        Ok(PreparedInstall {
            identity,
            manifest_bytes: request.manifest.clone(),
            package: request.package.clone(),
            package_hash,
        })
    }

    fn stage_package(
        &self,
        prepared: &PreparedInstall,
        cancel: &CancellationToken,
    ) -> Result<PathBuf, InstallError> {
        cancel_check(cancel)?;
        let staging = self.new_staging_dir()?;
        reject_symlink(&staging)?;
        write_file(
            &staging.join(MANIFEST_FILE),
            &prepared.manifest_bytes,
            cancel,
        )?;
        write_file(&staging.join(PACKAGE_FILE), &prepared.package, cancel)?;
        Ok(staging)
    }

    fn publish(
        &self,
        staging: &Path,
        dest: &Path,
        plugin: &PluginId,
        cancel: &CancellationToken,
    ) -> Result<(), InstallError> {
        cancel_check(cancel)?;
        reject_symlink(staging)?;
        if dest.exists() {
            reject_symlink(dest)?;
            let prev = self.rollback_dir(plugin);
            if prev.exists() {
                reject_symlink(&prev)?;
                remove_tree(&prev)?;
            }
            fs::rename(dest, &prev)?;
        }
        match fs::rename(staging, dest) {
            Ok(()) => {
                let prev = self.rollback_dir(plugin);
                if prev.exists() {
                    let _ = fs::remove_dir_all(&prev);
                }
                Ok(())
            }
            Err(err) => {
                let _ = self.restore_previous(plugin);
                Err(InstallError::Io(err))
            }
        }
    }

    fn record_quarantine(
        &self,
        manifest: &PluginManifest,
        request: &InstallRequest,
        package_hash: ArtifactId,
        cancel: &CancellationToken,
    ) -> Result<(), InstallError> {
        let mut identity = ExtensionIdentity::from_binding(&manifest.trust_binding());
        if identity.package_hash() != package_hash {
            identity = ExtensionIdentity::new(
                manifest.id().as_str(),
                &manifest.version().to_string(),
                manifest.publisher().as_str(),
                &package_hash.to_string(),
            )
            .map_err(InstallError::Trust)?;
        }
        if let Some(signature) = request.signature {
            identity = identity
                .with_signature(&signature.to_string())
                .map_err(InstallError::Trust)?;
        }
        let observation = ExtensionObservation::new(
            identity,
            request.source.clone(),
            request.scope,
            request.last_review.clone(),
        );
        // Untrusted observation only. Never register(): that path can rebind,
        // keep, or revoke a live grant for a still-published plugin (T-006).
        self.trust
            .observe_untrusted(&observation, cancel)
            .map_err(InstallError::Trust)?;
        Ok(())
    }

    fn load_installed(
        &self,
        plugin: &PluginId,
        cancel: &CancellationToken,
    ) -> Result<Option<InstalledPlugin>, InstallError> {
        cancel_check(cancel)?;
        let dest = self.installed_dir(plugin);
        match fs::symlink_metadata(&dest) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(InstallError::Io(err)),
            Ok(meta) if meta.file_type().is_symlink() => return Err(InstallError::UnsafePath),
            Ok(meta) if !meta.is_dir() => return Ok(None),
            Ok(_) => {}
        }
        let manifest_bytes = read_bounded(&dest.join(MANIFEST_FILE), self.max_package_bytes)?;
        let package = read_bounded(&dest.join(PACKAGE_FILE), self.max_package_bytes)?;
        let manifest = parse_manifest(&manifest_bytes, cancel).map_err(map_manifest)?;
        if manifest.id() != plugin {
            return Err(InstallError::HashMismatch);
        }
        let package_hash = ArtifactId::from_bytes(&package);
        if package_hash != manifest.digest() {
            return Err(InstallError::HashMismatch);
        }
        let identity = ExtensionIdentity::from_binding(&manifest.trust_binding());
        Ok(Some(InstalledPlugin {
            identity,
            manifest,
            path: dest,
        }))
    }

    fn restore_orphaned_rollbacks(&self, cancel: &CancellationToken) -> Result<(), InstallError> {
        cancel_check(cancel)?;
        let entries = match fs::read_dir(&self.rollback) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(InstallError::Io(err)),
        };
        for (i, entry) in entries.enumerate() {
            if i % CANCEL_STRIDE == 0 {
                cancel_check(cancel)?;
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(plugin) = name.to_str().and_then(|raw| PluginId::parse(raw).ok()) else {
                let _ = fs::remove_dir_all(entry.path());
                continue;
            };
            let dest = self.installed_dir(&plugin);
            if dest.exists() {
                let _ = fs::remove_dir_all(entry.path());
            } else {
                fs::rename(entry.path(), dest)?;
            }
        }
        Ok(())
    }

    fn restore_previous(&self, plugin: &PluginId) -> io::Result<()> {
        let prev = self.rollback_dir(plugin);
        let dest = self.installed_dir(plugin);
        if !prev.exists() {
            return Ok(());
        }
        if dest.exists() {
            let _ = fs::remove_dir_all(&dest);
        }
        fs::rename(prev, dest)
    }

    fn delete_published(&self, plugin: &PluginId) -> Result<(), InstallError> {
        let dest = self.installed_dir(plugin);
        if dest.exists() {
            reject_symlink(&dest)?;
            remove_tree(&dest)?;
        }
        let prev = self.rollback_dir(plugin);
        if prev.exists() {
            let _ = fs::remove_dir_all(prev);
        }
        Ok(())
    }

    fn purge_staging(&self) {
        if let Ok(entries) = fs::read_dir(&self.staging) {
            for entry in entries.flatten() {
                let _ = fs::remove_dir_all(entry.path());
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn ensure_layout(&self) -> Result<(), InstallError> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.installed)?;
        fs::create_dir_all(&self.staging)?;
        fs::create_dir_all(&self.rollback)?;
        Ok(())
    }

    fn new_staging_dir(&self) -> Result<PathBuf, InstallError> {
        let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = self.staging.join(format!("{}-{seq}", std::process::id()));
        if path.exists() {
            remove_tree(&path)?;
        }
        fs::create_dir_all(&path)?;
        reject_symlink(&path)?;
        Ok(path)
    }

    fn installed_dir(&self, plugin: &PluginId) -> PathBuf {
        self.installed.join(plugin.as_str())
    }

    fn rollback_dir(&self, plugin: &PluginId) -> PathBuf {
        self.rollback.join(plugin.as_str())
    }
}

impl Debug for PluginInstaller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginInstaller")
            .field("root", &self.root)
            .finish()
    }
}

impl InstallError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::HashMismatch
            | Self::Quarantined
            | Self::PolicyDenied
            | Self::UnsafePath
            | Self::PackageTooLarge { .. } => Some(ErrorCode::PluginCapabilityDenied),
            Self::NotInstalled => Some(ErrorCode::ConfigInvalid),
            Self::Manifest(err) => err.code(),
            Self::Trust(err) => err.code(),
            Self::Io(_) => Some(ErrorCode::InternalUnexpected),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = self.as_str();
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "plugin install operation cancelled",
            Self::PackageTooLarge { .. } => "plugin package exceeds the configured bound",
            Self::HashMismatch => "plugin package hash does not match the declared digest",
            Self::Quarantined => "plugin package signature failed and the install was quarantined",
            Self::PolicyDenied => "plugin requested capabilities are outside install policy",
            Self::NotInstalled => "plugin is not installed",
            Self::UnsafePath => "plugin install path is not a regular directory",
            Self::Manifest(_) => "plugin manifest is invalid",
            Self::Trust(_) => "plugin trust catalog rejected the install",
            Self::Io(_) => "plugin install I/O failed",
        }
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(err) => fmt::Display::fmt(err, f),
            Self::Trust(err) => fmt::Display::fmt(err, f),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Error for InstallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Manifest(err) => Some(err),
            Self::Trust(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl PartialEq for InstallError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Cancelled, Self::Cancelled)
            | (Self::HashMismatch, Self::HashMismatch)
            | (Self::Quarantined, Self::Quarantined)
            | (Self::PolicyDenied, Self::PolicyDenied)
            | (Self::NotInstalled, Self::NotInstalled)
            | (Self::UnsafePath, Self::UnsafePath) => true,
            (
                Self::PackageTooLarge {
                    limit: a_limit,
                    observed: a_obs,
                },
                Self::PackageTooLarge {
                    limit: b_limit,
                    observed: b_obs,
                },
            ) => a_limit == b_limit && a_obs == b_obs,
            (Self::Manifest(a), Self::Manifest(b)) => a == b,
            (Self::Trust(a), Self::Trust(b)) => a == b,
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

impl Eq for InstallError {}

impl From<io::Error> for InstallError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

struct PreparedInstall {
    identity: ExtensionIdentity,
    manifest_bytes: Vec<u8>,
    package: Vec<u8>,
    package_hash: ArtifactId,
}

fn receipt_from(
    prepared: &PreparedInstall,
    dest: &Path,
    record: &ExtensionTrustRecord,
) -> InstallReceipt {
    InstallReceipt {
        identity: prepared.identity.clone(),
        source: record.source().clone(),
        scope: record.scope(),
        package_hash: prepared.package_hash,
        path: dest.to_path_buf(),
        executable_enabled: record.executable_enabled(),
    }
}

fn check_policy(manifest: &PluginManifest, policy: &InstallPolicy) -> Result<(), InstallError> {
    let Some(allowlist) = policy.allowlist.as_ref() else {
        return Ok(());
    };
    for requested in manifest.requested_caps() {
        let asked = TrustedCapability::new(requested.capability(), requested.resource().clone())
            .map_err(InstallError::Trust)?;
        if !allowlist.iter().any(|granted| granted == &asked) {
            return Err(InstallError::PolicyDenied);
        }
    }
    Ok(())
}

fn map_manifest(err: ManifestError) -> InstallError {
    match err {
        ManifestError::Cancelled => InstallError::Cancelled,
        other => InstallError::Manifest(other),
    }
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), InstallError> {
    cancel.check().map_err(|_| InstallError::Cancelled)
}

fn reject_symlink(path: &Path) -> Result<(), InstallError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(InstallError::UnsafePath),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(InstallError::Io(err)),
    }
}

fn write_file(path: &Path, bytes: &[u8], cancel: &CancellationToken) -> Result<(), InstallError> {
    cancel_check(cancel)?;
    reject_symlink(path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut offset = 0;
    while offset < bytes.len() {
        if offset % (WRITE_CHUNK * CANCEL_STRIDE) == 0 {
            cancel_check(cancel)?;
        }
        let end = (offset + WRITE_CHUNK).min(bytes.len());
        file.write_all(&bytes[offset..end])?;
        offset = end;
    }
    file.sync_all()?;
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, InstallError> {
    reject_symlink(path)?;
    let metadata = fs::metadata(path)?;
    let observed = metadata.len();
    if observed > limit as u64 {
        return Err(InstallError::PackageTooLarge {
            limit,
            observed: observed as usize,
        });
    }
    let bytes = fs::read(path)?;
    if bytes.len() > limit {
        return Err(InstallError::PackageTooLarge {
            limit,
            observed: bytes.len(),
        });
    }
    Ok(bytes)
}

fn remove_tree(path: &Path) -> Result<(), InstallError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(InstallError::Io(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use capability_broker::{Capability, FilesystemScope, ResourceDescriptor};
    use protocol::ArtifactId;

    use crate::manifest::SemVer;
    use crate::skills::ProjectTrust;
    use crate::trust::{
        ExtensionIdentity, ExtensionTrustStatus, InstallSourceKind, TrustGrant, TrustedCapability,
    };
    use crate::wasm::instantiate;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    const SECRET: &str = "super-secret-password";
    const EVIL_LOCATOR: &str = "https://evil.example/plugin?token=super-secret-password";
    const REVIEW: &str = "2026-01-15T12:00:00Z";
    const REVIEW_LATER: &str = "2026-02-01T00:00:00Z";
    const SIG_A: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const SIG_B: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    struct TempRoot {
        dir: PathBuf,
    }

    impl TempRoot {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rapidlm-plugin-install-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp install root");
            Self { dir }
        }

        fn installer(&self) -> PluginInstaller {
            PluginInstaller::open(&self.dir).expect("open installer")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn review() -> ReviewTimestamp {
        ReviewTimestamp::parse(REVIEW).expect("review")
    }

    fn later() -> ReviewTimestamp {
        ReviewTimestamp::parse(REVIEW_LATER).expect("later")
    }

    fn source() -> InstallSource {
        InstallSource::new(InstallSourceKind::Marketplace, EVIL_LOCATOR).expect("source")
    }

    fn package_bytes() -> Vec<u8> {
        b"not-a-wasm-module-fixture-plugin".to_vec()
    }

    fn digest(bytes: &[u8]) -> ArtifactId {
        ArtifactId::from_bytes(bytes)
    }

    fn signature(raw: &str) -> ArtifactId {
        raw.parse().expect("signature pin")
    }

    fn manifest_json(package: &[u8]) -> String {
        manifest_json_version(package, "1.2.3")
    }

    fn manifest_json_version(package: &[u8], version: &str) -> String {
        manifest_json_id_version("acme.fmt", package, version)
    }

    fn manifest_json_id_version(id: &str, package: &[u8], version: &str) -> String {
        format!(
            r#"{{"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"{id}","version":"{version}","publisher":"acme","digest":"{digest}","entrypoint":"plugin.wasm#rapidlm:plugin/tool@1.0.0","wit_version":"1.0.0","compatibility":{{"min":"1.0.0","max":"2.0.0"}},"requested_caps":[{{"capability":{{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"}},"resource":{{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}}}],"skills":["fmt"],"hooks":[],"mcp_servers":[]}}"#,
            digest = digest(package)
        )
    }

    fn pinned_source() -> InstallSource {
        InstallSource::new(InstallSourceKind::Marketplace, "marketplace:acme.fmt").expect("pinned")
    }

    fn request_at(
        package: &[u8],
        version: &str,
        src: InstallSource,
        stamp: ReviewTimestamp,
    ) -> InstallRequest {
        InstallRequest::new(
            manifest_json_version(package, version).into_bytes(),
            package.to_vec(),
            src,
            TrustScope::User,
            stamp,
        )
    }

    fn request(package: &[u8]) -> InstallRequest {
        InstallRequest::new(
            manifest_json(package).into_bytes(),
            package.to_vec(),
            source(),
            TrustScope::User,
            review(),
        )
    }

    fn read_cap() -> TrustedCapability {
        TrustedCapability::new(
            Capability::FsRead,
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/**/*.rs").expect("fs")),
        )
        .expect("read")
    }

    fn invoke_cap() -> TrustedCapability {
        TrustedCapability::new(
            Capability::PluginInvoke,
            ResourceDescriptor::Plugin(
                capability_broker::PluginScope::new("acme.fmt", "format").expect("plugin scope"),
            ),
        )
        .expect("invoke")
    }

    #[test]
    fn install_publishes_without_enabling_executable() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        let receipt = installer
            .install(&request(&package), &live())
            .expect("install");
        assert_eq!(receipt.identity().plugin().as_str(), "acme.fmt");
        assert_eq!(receipt.package_hash(), digest(&package));
        assert!(!receipt.executable_enabled());
        assert!(installer.is_active("acme.fmt", &live()).expect("active"));
        assert_eq!(
            installer
                .trust()
                .inspect("acme.fmt", &live())
                .expect("inspect")
                .status(),
            ExtensionTrustStatus::Untrusted
        );
        assert_eq!(
            installer.trust().authorize_executable(
                receipt.identity(),
                crate::skills::ProjectTrust::Trusted,
                &live()
            ),
            Err(TrustError::Untrusted)
        );
    }

    #[test]
    fn install_never_executes_non_wasm_package() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        installer
            .install(&request(&package), &live())
            .expect("install garbage bytes");
        let installed = installer.get("acme.fmt", &live()).expect("get");
        let err = instantiate(&package, installed.manifest(), &[], &live())
            .expect_err("install must not execute the package");
        assert_ne!(format!("{err}"), "");
        assert!(!format!("{err}").contains(SECRET));
    }

    #[test]
    fn leftover_staging_is_not_active() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let staging = installer.staging.join("orphan");
        fs::create_dir_all(&staging).expect("staging");
        fs::write(staging.join(PACKAGE_FILE), package_bytes()).expect("pkg");
        fs::write(staging.join(MANIFEST_FILE), b"{}").expect("manifest");
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
        installer.recover(&live()).expect("recover");
        assert!(!staging.exists());
        assert!(!installer.installed.join("acme.fmt").exists());
    }

    #[test]
    fn cancelled_install_leaves_no_active_plugin() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let cancel = live();
        cancel.cancel();
        assert_eq!(
            installer.install(&request(&package_bytes()), &cancel),
            Err(InstallError::Cancelled)
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
        assert!(installer
            .staging
            .read_dir()
            .expect("staging")
            .next()
            .is_none());
    }

    #[test]
    fn hash_mismatch_fails_closed_and_does_not_publish() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        let mut json = manifest_json(&package);
        json = json.replace(
            &digest(&package).to_string(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let req = InstallRequest::new(
            json.into_bytes(),
            package,
            source(),
            TrustScope::User,
            review(),
        );
        assert_eq!(
            installer.install(&req, &live()),
            Err(InstallError::HashMismatch)
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
        assert!(!installer.trust().catalog_path().exists());
    }

    #[test]
    fn policy_pin_hash_mismatch_is_rejected() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        let other = ArtifactId::from_bytes(b"other-package");
        let req =
            request(&package).with_policy(InstallPolicy::manifest_only().with_expected_hash(other));
        assert_eq!(
            installer.install(&req, &live()),
            Err(InstallError::HashMismatch)
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
    }

    #[test]
    fn signature_mismatch_quarantines_without_activating() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        let req = request(&package)
            .with_signature(signature(SIG_B))
            .with_policy(InstallPolicy::manifest_only().with_expected_signature(signature(SIG_A)));
        assert_eq!(
            installer.install(&req, &live()),
            Err(InstallError::Quarantined)
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
        let view = installer
            .trust()
            .inspect("acme.fmt", &live())
            .expect("quarantine audit");
        assert_eq!(view.status(), ExtensionTrustStatus::Untrusted);
        assert!(!view.executable_enabled());
        assert_eq!(view.source().locator(), EVIL_LOCATOR);
    }

    #[test]
    fn missing_required_signature_is_quarantined() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let req = request(&package_bytes())
            .with_policy(InstallPolicy::manifest_only().require_signature());
        assert_eq!(
            installer.install(&req, &live()),
            Err(InstallError::Quarantined)
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
    }

    #[test]
    fn signature_failure_does_not_rebind_live_version_range_grant() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let first = package_bytes();
        let live_receipt = installer
            .install(
                &request_at(&first, "1.2.3", pinned_source(), review()),
                &live(),
            )
            .expect("publish live");
        let grant = TrustGrant::new(
            live_receipt.identity().clone(),
            pinned_source(),
            TrustScope::User,
            review(),
        )
        .with_version_range(SemVer::new(1, 0, 0), SemVer::new(2, 0, 0))
        .expect("range")
        .with_granted_capabilities([invoke_cap()])
        .expect("caps");
        installer.trust().grant(&grant, &live()).expect("grant");
        installer
            .trust()
            .authorize_executable(live_receipt.identity(), ProjectTrust::Trusted, &live())
            .expect("live grant");

        let mismatched = b"rejected-signature-mismatch-bytes".to_vec();
        let mismatch_req = request_at(&mismatched, "1.4.0", source(), later())
            .with_signature(signature(SIG_B))
            .with_policy(InstallPolicy::manifest_only().with_expected_signature(signature(SIG_A)));
        assert_eq!(
            installer.install(&mismatch_req, &live()),
            Err(InstallError::Quarantined)
        );

        let unsigned = b"rejected-missing-required-signature".to_vec();
        let missing_req = request_at(&unsigned, "1.5.0", source(), later())
            .with_policy(InstallPolicy::manifest_only().require_signature());
        assert_eq!(
            installer.install(&missing_req, &live()),
            Err(InstallError::Quarantined)
        );

        let kept = installer.get("acme.fmt", &live()).expect("still published");
        assert_eq!(kept.identity().package_hash(), digest(&first));
        let view = installer
            .trust()
            .inspect("acme.fmt", &live())
            .expect("live grant intact");
        assert_eq!(view.package_hash(), digest(&first));
        assert_eq!(view.source().locator(), "marketplace:acme.fmt");
        assert_eq!(view.status(), ExtensionTrustStatus::Trusted);
        assert!(view.executable_enabled());
        assert_ne!(view.source().locator(), EVIL_LOCATOR);

        let rejected_mismatch = ExtensionIdentity::new(
            "acme.fmt",
            "1.4.0",
            "acme",
            &digest(&mismatched).to_string(),
        )
        .expect("rejected mismatch")
        .with_signature(SIG_B)
        .expect("sig");
        assert_eq!(
            installer.trust().authorize_executable(
                &rejected_mismatch,
                ProjectTrust::Trusted,
                &live()
            ),
            Err(TrustError::Untrusted)
        );

        let rejected_missing = ExtensionIdentity::new(
            "acme.fmt",
            "1.5.0",
            "acme",
            &digest(&unsigned).to_string(),
        )
        .expect("rejected missing");
        assert_eq!(
            installer.trust().authorize_executable(
                &rejected_missing,
                ProjectTrust::Trusted,
                &live()
            ),
            Err(TrustError::Untrusted)
        );

        installer
            .trust()
            .authorize_executable(live_receipt.identity(), ProjectTrust::Trusted, &live())
            .expect("live grant still authorized");
        let after = installer
            .trust()
            .inspect("acme.fmt", &live())
            .expect("no rewrite after authorize");
        assert_eq!(after.package_hash(), digest(&first));
        assert_eq!(after.source().locator(), "marketplace:acme.fmt");
        assert_ne!(after.package_hash(), digest(&mismatched));
        assert_ne!(after.package_hash(), digest(&unsigned));
    }

    #[test]
    fn policy_allowlist_denies_undeclared_capability() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let req = request(&package_bytes()).with_policy(
            InstallPolicy::manifest_only()
                .with_allowlist([invoke_cap()])
                .expect("allow"),
        );
        assert_eq!(
            installer.install(&req, &live()),
            Err(InstallError::PolicyDenied)
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
    }

    #[test]
    fn ambient_host_capability_cannot_bypass_install() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        let json = manifest_json(&package)
            .replace(r#""root":"repo""#, r#""root":"host""#)
            .replace(r#""glob":"src/**/*.rs""#, r#""glob":"/etc/passwd""#);
        let req = InstallRequest::new(
            json.into_bytes(),
            package,
            source(),
            TrustScope::User,
            review(),
        );
        assert_eq!(
            installer.install(&req, &live()),
            Err(InstallError::Manifest(ManifestError::AmbientHostFilesystem))
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
    }

    #[test]
    fn failed_upgrade_keeps_previous_active() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let first = package_bytes();
        installer.install(&request(&first), &live()).expect("first");
        let second = b"second-package-bytes".to_vec();
        let mut json = manifest_json(&second);
        json = json.replace(
            &digest(&second).to_string(),
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let bad = InstallRequest::new(
            json.into_bytes(),
            second,
            source(),
            TrustScope::User,
            later(),
        );
        assert_eq!(
            installer.install(&bad, &live()),
            Err(InstallError::HashMismatch)
        );
        let kept = installer.get("acme.fmt", &live()).expect("kept");
        assert_eq!(kept.identity().package_hash(), digest(&first));
        assert!(installer.is_active("acme.fmt", &live()).expect("active"));
    }

    #[test]
    fn replace_install_swaps_package_atomically() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let first = package_bytes();
        installer.install(&request(&first), &live()).expect("first");
        let second = b"replacement-package-bytes".to_vec();
        let receipt = installer
            .install(&request(&second), &live())
            .expect("replace");
        assert_eq!(receipt.package_hash(), digest(&second));
        assert!(!installer.rollback.join("acme.fmt").exists());
        assert_eq!(
            installer
                .get("acme.fmt", &live())
                .expect("get")
                .identity()
                .package_hash(),
            digest(&second)
        );
    }

    #[test]
    fn orphaned_rollback_is_restored_and_staging_is_dropped() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let first = package_bytes();
        installer.install(&request(&first), &live()).expect("first");
        let dest = installer.installed.join("acme.fmt");
        let prev = installer.rollback.join("acme.fmt");
        fs::rename(&dest, &prev).expect("simulate crash after dest move");
        let orphan = installer.staging.join("partial");
        fs::create_dir_all(&orphan).expect("orphan");
        fs::write(orphan.join(PACKAGE_FILE), b"partial").expect("partial");
        installer.recover(&live()).expect("recover");
        assert!(installer.is_active("acme.fmt", &live()).expect("restored"));
        assert!(!orphan.exists());
        assert_eq!(
            installer
                .get("acme.fmt", &live())
                .expect("get")
                .identity()
                .package_hash(),
            digest(&first)
        );
    }

    #[test]
    fn remove_deletes_files_and_preserves_audit_record() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        let receipt = installer
            .install(&request(&package), &live())
            .expect("install");
        let grant = TrustGrant::new(
            receipt.identity().clone(),
            source(),
            TrustScope::User,
            review(),
        )
        .with_granted_capabilities([read_cap()])
        .expect("caps");
        installer.trust().grant(&grant, &live()).expect("grant");
        assert_eq!(
            installer
                .trust()
                .inspect("acme.fmt", &live())
                .expect("granted")
                .status(),
            ExtensionTrustStatus::Trusted
        );
        let removed = installer
            .remove("acme.fmt", later(), &live())
            .expect("remove");
        assert!(!installer.is_active("acme.fmt", &live()).expect("gone"));
        assert!(!installer.installed.join("acme.fmt").exists());
        assert_eq!(removed.audit().status(), ExtensionTrustStatus::Untrusted);
        assert!(!removed.audit().executable_enabled());
        assert_eq!(removed.audit().last_review().as_str(), REVIEW_LATER);
        assert_eq!(removed.audit().source().locator(), EVIL_LOCATOR);
        let listed = installer.trust().list(&live()).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].plugin().as_str(), "acme.fmt");
    }

    #[test]
    fn source_locator_is_data_and_never_echoed_or_fetched() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let err = installer
            .install(
                &request(&package_bytes()).with_policy(
                    InstallPolicy::manifest_only().with_expected_signature(signature(SIG_A)),
                ),
                &live(),
            )
            .expect_err("quarantine");
        let text = err.to_string();
        assert!(!text.contains(SECRET));
        assert!(!text.contains(EVIL_LOCATOR));
        assert!(!text.contains("https://"));
        assert!(!tmp.dir.join("evil.example").exists());
        assert!(Path::new(EVIL_LOCATOR).is_absolute() || EVIL_LOCATOR.contains("://"));
        assert!(!installer.root().join(EVIL_LOCATOR).exists());
    }

    #[test]
    fn package_over_size_bound_is_rejected() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let mut huge = request(&package_bytes());
        huge.package = vec![0u8; MAX_PACKAGE_BYTES + 1];
        assert_eq!(
            installer.install(&huge, &live()),
            Err(InstallError::PackageTooLarge {
                limit: MAX_PACKAGE_BYTES,
                observed: MAX_PACKAGE_BYTES + 1
            })
        );
        assert!(!installer.is_active("acme.fmt", &live()).expect("query"));
    }

    #[test]
    fn symlink_install_target_is_rejected() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let outside = tmp.dir.join("outside");
        fs::create_dir_all(&outside).expect("outside");
        let dest = installer.installed.join("acme.fmt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &dest).expect("symlink");
            assert_eq!(
                installer.install(&request(&package_bytes()), &live()),
                Err(InstallError::UnsafePath)
            );
            assert!(fs::symlink_metadata(&dest)
                .expect("meta")
                .file_type()
                .is_symlink());
            assert!(outside.read_dir().expect("outside").next().is_none());
        }
    }

    #[test]
    fn tampered_package_is_not_active() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        installer
            .install(&request(&package_bytes()), &live())
            .expect("install");
        fs::write(
            installer.installed.join("acme.fmt").join(PACKAGE_FILE),
            b"tampered",
        )
        .expect("tamper");
        assert_eq!(
            installer.get("acme.fmt", &live()).expect_err("tamper"),
            InstallError::HashMismatch
        );
    }

    #[test]
    fn display_does_not_echo_plugin_or_locator() {
        let err = InstallError::Quarantined;
        let text = err.to_string();
        assert!(!text.contains(SECRET));
        assert!(!text.contains(EVIL_LOCATOR));
        assert!(!text.contains("acme.fmt"));
    }

    #[test]
    fn remove_missing_plugin_fails_closed() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        assert_eq!(
            installer.remove("acme.fmt", review(), &live()),
            Err(InstallError::NotInstalled)
        );
    }

    #[test]
    fn update_replaces_installed_plugin_and_fails_closed_when_absent() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let v1 = package_bytes();
        installer
            .install(&request_at(&v1, "1.2.3", source(), review()), &live())
            .expect("install v1");

        // Update on a plugin that was never installed must fail closed.
        let absent_manifest =
            manifest_json_id_version("other.absent", b"other-package", "9.9.9").into_bytes();
        let missing = InstallRequest::new(
            absent_manifest,
            b"other-package".to_vec(),
            pinned_source(),
            TrustScope::User,
            review(),
        );
        assert!(matches!(
            installer.update(&missing, &live()),
            Err(InstallError::NotInstalled)
        ));

        let v2 = b"not-a-wasm-module-fixture-plugin-v2".to_vec();
        let receipt = installer
            .update(
                &request_at(&v2, "1.3.0", pinned_source(), later()),
                &live(),
            )
            .expect("update");
        assert_eq!(receipt.package_hash(), digest(&v2));
        assert!(installer.is_active("acme.fmt", &live()).expect("active"));
        let installed = installer.get("acme.fmt", &live()).expect("get");
        assert_eq!(
            installed.manifest().version().to_string(),
            SemVer::parse("1.3.0").expect("ver").to_string()
        );
    }

    #[test]
    fn disable_and_enable_transition_active_state_without_deleting() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        let package = package_bytes();
        installer
            .install(&request(&package), &live())
            .expect("install");
        assert!(installer.is_active("acme.fmt", &live()).expect("active"));

        let off = installer.disable("acme.fmt", &live()).expect("disable");
        assert!(!off.enabled());
        assert!(!installer.is_active("acme.fmt", &live()).expect("active"));
        // Disabled tree stays inspectable and re-disable is idempotent.
        installer.get("acme.fmt", &live()).expect("inspectable");
        let off_again = installer.disable("acme.fmt", &live()).expect("idempotent");
        assert!(!off_again.enabled());

        let on = installer.enable("acme.fmt", &live()).expect("enable");
        assert!(on.enabled());
        assert!(installer.is_active("acme.fmt", &live()).expect("active"));
        // Enable without prior disable is idempotent.
        installer.enable("acme.fmt", &live()).expect("idempotent");

        // Lifecycle ops on an absent plugin fail closed.
        assert!(matches!(
            installer.disable("nope.absent", &live()),
            Err(InstallError::NotInstalled)
        ));
    }

    #[test]
    fn disabled_plugin_stays_disabled_across_update_then_enables_cleanly() {
        let tmp = TempRoot::create();
        let installer = tmp.installer();
        installer
            .install(&request(&package_bytes()), &live())
            .expect("install");
        installer.disable("acme.fmt", &live()).expect("disable");
        let v2 = b"updated-package-bytes-v2".to_vec();
        installer
            .update(
                &request_at(&v2, "2.0.0", pinned_source(), later()),
                &live(),
            )
            .expect("update while disabled");
        assert!(
            !installer.is_active("acme.fmt", &live()).expect("active"),
            "update must not silently re-enable a disabled plugin"
        );
        installer.enable("acme.fmt", &live()).expect("enable");
        assert!(installer.is_active("acme.fmt", &live()).expect("active"));
    }
}
