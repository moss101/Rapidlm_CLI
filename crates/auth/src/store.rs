//! Credential provider trait and platform-keychain adapter boundary.
//!
//! `put`/`get`/`delete`/`list_metadata` return [`SecretRef`] handles or redacted
//! [`SecretValue`] buffers. [`CredentialMetadata`] never contains secret bytes.
//! Encrypted-file fallback is used only after explicit setup; keychain unavailability
//! otherwise blocks persistence.

use std::fmt::{self, Debug, Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering, compiler_fence};
use std::sync::{Arc, Mutex};

use serde::de::{Error as DeError, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::secret::{
    MAX_SECRET_BYTES, MAX_SECRET_REF_ALIAS_BYTES, SecretError, SecretRef, SecretValue,
};

/// OS keychain service name for RapidLM credentials.
pub const KEYCHAIN_SERVICE: &str = "rapidlm.credentials";

/// Maximum credentials returned by [`CredentialStore::list_metadata`].
pub const MAX_METADATA_LIST: usize = 1024;

/// Maximum accepted provider identifier length.
pub const MAX_PROVIDER_BYTES: usize = 64;

/// Maximum accepted keychain account length.
pub const MAX_KEYCHAIN_ACCOUNT_BYTES: usize = 256;

const CREDENTIAL_METADATA_SCHEMA: &str = "rapidlm.auth.credential_metadata";
const SCHEMA_VERSION: u32 = 1;

/// Cooperative cancellation for store operations.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Typed failures for credential-store operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    Cancelled,
    NotFound,
    PersistenceBlocked { reason: PersistenceBlockReason },
    BoundExceeded { limit: usize, requested: usize },
    InvalidHandle,
    InvalidMetadata,
    InvalidProvider,
    LockPoisoned,
    Secret(SecretError),
}

/// Why durable persistence was refused. Never silently degraded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceBlockReason {
    KeychainUnavailable,
    EncryptedFallbackNotConfigured,
}

/// Kind of stored credential. Metadata only; never a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ProviderApiKey,
    OauthRefresh,
    OauthAccess,
    DaemonToken,
    WorkerCert,
    Generic,
}

/// Whether a put may touch a durable backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    Ephemeral,
    Persist,
}

/// Backend that currently holds a credential record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialBackendKind {
    Memory,
    PlatformKeychain,
    EncryptedFallback,
}

/// Detected OS credential provider. Availability is separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PlatformKeychainKind {
    MacosKeychain,
    WindowsCredentialManager,
    FreedesktopSecretService,
    File,
    Unsupported,
}

/// Probe result for a [`PlatformKeychain`] implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeychainProbe {
    available: bool,
    kind: PlatformKeychainKind,
}

/// Handle-only credential descriptor. Never carries secret bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialMetadata {
    refer: SecretRef,
    kind: CredentialKind,
    provider: Option<String>,
    backend: CredentialBackendKind,
    retention: Retention,
    byte_len: usize,
}

/// Insert or replace a credential. The value stays inside [`SecretValue`].
pub struct CredentialPut {
    value: SecretValue,
    kind: CredentialKind,
    provider: Option<String>,
    retention: Retention,
}

/// Filter for [`CredentialStore::list_metadata`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataQuery {
    provider: Option<String>,
    kind: Option<CredentialKind>,
    limit: usize,
}

/// Durable or in-process credential provider.
pub trait CredentialStore: Send + Sync {
    fn put(&self, put: CredentialPut, cancel: &CancellationToken) -> Result<SecretRef, StoreError>;

    fn get(&self, refer: &SecretRef, cancel: &CancellationToken)
    -> Result<SecretValue, StoreError>;

    fn delete(&self, refer: &SecretRef, cancel: &CancellationToken) -> Result<(), StoreError>;

    fn list_metadata(
        &self,
        query: &MetadataQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CredentialMetadata>, StoreError>;
}

/// In-memory provider used by tests and ephemeral process-local storage.
pub struct InMemoryCredentialStore {
    table: MemoryTable,
}

/// OS keychain/keyring operations. Implementations must not log secret bytes.
pub trait PlatformKeychain: Send + Sync {
    fn probe(&self) -> KeychainProbe;

    fn put(
        &self,
        item: &KeychainItemMeta,
        secret: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), StoreError>;

    fn get(
        &self,
        item: &KeychainItemMeta,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, StoreError>;

    fn delete(&self, item: &KeychainItemMeta, cancel: &CancellationToken)
    -> Result<(), StoreError>;

    fn list(&self, cancel: &CancellationToken) -> Result<Vec<KeychainItemMeta>, StoreError>;
}

/// Handle-only keychain item. No password/secret field exists.
#[derive(Clone, Eq, PartialEq)]
pub struct KeychainItemMeta {
    account: String,
    service: String,
    refer: SecretRef,
    kind: CredentialKind,
    provider: Option<String>,
    byte_len: usize,
}

/// Fail-closed stand-in until a real OS adapter is wired.
pub struct UnimplementedPlatformKeychain;

/// Explicit encrypted-file fallback. Not used unless [`EncryptedFallback::is_explicitly_configured`].
pub trait EncryptedFallback: Send + Sync {
    fn is_explicitly_configured(&self) -> bool;

    fn put(&self, put: &CredentialPut, cancel: &CancellationToken)
    -> Result<SecretRef, StoreError>;

    fn get(&self, refer: &SecretRef, cancel: &CancellationToken)
    -> Result<SecretValue, StoreError>;

    fn delete(&self, refer: &SecretRef, cancel: &CancellationToken) -> Result<(), StoreError>;

    fn list_metadata(
        &self,
        query: &MetadataQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CredentialMetadata>, StoreError>;
}

/// Default fallback: persistence stays blocked when the keychain is down.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEncryptedFallback;

/// Platform-keychain-backed store. Encrypted fallback is opt-in only.
pub struct PlatformKeychainAdapter<K, F = NoEncryptedFallback> {
    keychain: K,
    fallback: F,
    ephemeral: MemoryTable,
}

struct MemoryTable {
    records: Mutex<Vec<StoredCredential>>,
}

struct StoredCredential {
    refer: SecretRef,
    bytes: Vec<u8>,
    kind: CredentialKind,
    provider: Option<String>,
    backend: CredentialBackendKind,
    retention: Retention,
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

    pub fn check(&self) -> Result<(), StoreError> {
        if self.is_cancelled() {
            Err(StoreError::Cancelled)
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

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "auth.cancelled",
            Self::NotFound => "auth.credential_not_found",
            Self::PersistenceBlocked { reason } => match reason {
                PersistenceBlockReason::KeychainUnavailable => "auth.keychain_unavailable",
                PersistenceBlockReason::EncryptedFallbackNotConfigured => {
                    "auth.encrypted_fallback_not_configured"
                }
            },
            Self::BoundExceeded { .. } => "auth.credential_bound_exceeded",
            Self::InvalidHandle => "auth.credential_handle_invalid",
            Self::InvalidMetadata => "auth.credential_metadata_invalid",
            Self::InvalidProvider => "auth.credential_provider_invalid",
            Self::LockPoisoned => "auth.credential_lock_poisoned",
            Self::Secret(err) => err.code(),
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for StoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("credential-store operation was cancelled"),
            Self::NotFound => f.write_str("credential was not found"),
            Self::PersistenceBlocked { reason } => match reason {
                PersistenceBlockReason::KeychainUnavailable => {
                    f.write_str("credential persistence blocked: platform keychain unavailable")
                }
                PersistenceBlockReason::EncryptedFallbackNotConfigured => f.write_str(
                    "credential persistence blocked: encrypted fallback is not configured",
                ),
            },
            Self::BoundExceeded { limit, requested } => {
                write!(f, "credential-store bound exceeded ({requested} > {limit})")
            }
            Self::InvalidHandle => f.write_str("credential handle is invalid"),
            Self::InvalidMetadata => f.write_str("credential metadata is invalid"),
            Self::InvalidProvider => f.write_str("credential provider identifier is invalid"),
            Self::LockPoisoned => f.write_str("credential-store lock was poisoned"),
            Self::Secret(err) => Display::fmt(err, f),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<SecretError> for StoreError {
    fn from(err: SecretError) -> Self {
        match err {
            SecretError::Cancelled => Self::Cancelled,
            SecretError::BoundExceeded { limit, requested } => {
                Self::BoundExceeded { limit, requested }
            }
            SecretError::InvalidRef => Self::InvalidHandle,
            other => Self::Secret(other),
        }
    }
}

impl KeychainProbe {
    pub fn available(kind: PlatformKeychainKind) -> Self {
        Self {
            available: true,
            kind,
        }
    }

    pub fn unavailable(kind: PlatformKeychainKind) -> Self {
        Self {
            available: false,
            kind,
        }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn kind(&self) -> PlatformKeychainKind {
        self.kind
    }
}

/// Detect the platform keychain family. Does not claim the backend is usable.
pub fn platform_keychain_kind() -> PlatformKeychainKind {
    if cfg!(target_os = "macos") {
        PlatformKeychainKind::MacosKeychain
    } else if cfg!(target_os = "windows") {
        PlatformKeychainKind::WindowsCredentialManager
    } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        PlatformKeychainKind::FreedesktopSecretService
    } else {
        PlatformKeychainKind::Unsupported
    }
}

impl CredentialMetadata {
    pub const SCHEMA: &'static str = CREDENTIAL_METADATA_SCHEMA;
    pub const SCHEMA_VERSION: u32 = SCHEMA_VERSION;

    pub fn secret_ref(&self) -> &SecretRef {
        &self.refer
    }

    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn backend(&self) -> CredentialBackendKind {
        self.backend
    }

    pub fn retention(&self) -> Retention {
        self.retention
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }
}

impl CredentialPut {
    pub fn persist(value: SecretValue, kind: CredentialKind) -> Self {
        Self {
            value,
            kind,
            provider: None,
            retention: Retention::Persist,
        }
    }

    pub fn ephemeral(value: SecretValue, kind: CredentialKind) -> Self {
        Self {
            value,
            kind,
            provider: None,
            retention: Retention::Ephemeral,
        }
    }

    pub fn with_provider(mut self, provider: &str) -> Result<Self, StoreError> {
        self.provider = Some(parse_provider(provider)?);
        Ok(self)
    }

    pub fn secret_ref(&self) -> &SecretRef {
        self.value.secret_ref()
    }

    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn retention(&self) -> Retention {
        self.retention
    }

    pub fn byte_len(&self) -> usize {
        self.value.len()
    }
}

impl MetadataQuery {
    pub fn new() -> Self {
        Self {
            provider: None,
            kind: None,
            limit: MAX_METADATA_LIST,
        }
    }

    pub fn with_provider(mut self, provider: &str) -> Result<Self, StoreError> {
        self.provider = Some(parse_provider(provider)?);
        Ok(self)
    }

    pub fn with_kind(mut self, kind: CredentialKind) -> Self {
        self.kind = Some(kind);
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Result<Self, StoreError> {
        if limit > MAX_METADATA_LIST {
            return Err(StoreError::BoundExceeded {
                limit: MAX_METADATA_LIST,
                requested: limit,
            });
        }
        self.limit = limit;
        Ok(self)
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn kind(&self) -> Option<CredentialKind> {
        self.kind
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

impl Default for MetadataQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self {
            table: MemoryTable::new(),
        }
    }
}

impl Default for InMemoryCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn put(&self, put: CredentialPut, cancel: &CancellationToken) -> Result<SecretRef, StoreError> {
        self.table.put(put, CredentialBackendKind::Memory, cancel)
    }

    fn get(
        &self,
        refer: &SecretRef,
        cancel: &CancellationToken,
    ) -> Result<SecretValue, StoreError> {
        self.table.get(refer, cancel)
    }

    fn delete(&self, refer: &SecretRef, cancel: &CancellationToken) -> Result<(), StoreError> {
        self.table.delete(refer, cancel)
    }

    fn list_metadata(
        &self,
        query: &MetadataQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CredentialMetadata>, StoreError> {
        self.table.list_metadata(query, cancel)
    }
}

impl KeychainItemMeta {
    pub fn new(
        refer: SecretRef,
        kind: CredentialKind,
        provider: Option<String>,
    ) -> Result<Self, StoreError> {
        let account = keychain_account(&refer)?;
        Ok(Self {
            account,
            service: KEYCHAIN_SERVICE.to_string(),
            refer,
            kind,
            provider,
            byte_len: 0,
        })
    }

    pub fn with_byte_len(mut self, byte_len: usize) -> Result<Self, StoreError> {
        if byte_len > MAX_SECRET_BYTES {
            return Err(StoreError::BoundExceeded {
                limit: MAX_SECRET_BYTES,
                requested: byte_len,
            });
        }
        self.byte_len = byte_len;
        Ok(self)
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn secret_ref(&self) -> &SecretRef {
        &self.refer
    }

    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }
}

impl UnimplementedPlatformKeychain {
    pub fn new() -> Self {
        Self
    }
}

impl Default for UnimplementedPlatformKeychain {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformKeychain for UnimplementedPlatformKeychain {
    fn probe(&self) -> KeychainProbe {
        KeychainProbe::unavailable(platform_keychain_kind())
    }

    fn put(
        &self,
        _item: &KeychainItemMeta,
        _secret: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), StoreError> {
        cancel.check()?;
        Err(StoreError::PersistenceBlocked {
            reason: PersistenceBlockReason::KeychainUnavailable,
        })
    }

    fn get(
        &self,
        _item: &KeychainItemMeta,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, StoreError> {
        cancel.check()?;
        Err(StoreError::NotFound)
    }

    fn delete(
        &self,
        _item: &KeychainItemMeta,
        cancel: &CancellationToken,
    ) -> Result<(), StoreError> {
        cancel.check()?;
        Err(StoreError::NotFound)
    }

    fn list(&self, cancel: &CancellationToken) -> Result<Vec<KeychainItemMeta>, StoreError> {
        cancel.check()?;
        Ok(Vec::new())
    }
}

impl EncryptedFallback for NoEncryptedFallback {
    fn is_explicitly_configured(&self) -> bool {
        false
    }

    fn put(
        &self,
        _put: &CredentialPut,
        cancel: &CancellationToken,
    ) -> Result<SecretRef, StoreError> {
        cancel.check()?;
        Err(StoreError::PersistenceBlocked {
            reason: PersistenceBlockReason::EncryptedFallbackNotConfigured,
        })
    }

    fn get(
        &self,
        _refer: &SecretRef,
        cancel: &CancellationToken,
    ) -> Result<SecretValue, StoreError> {
        cancel.check()?;
        Err(StoreError::NotFound)
    }

    fn delete(&self, _refer: &SecretRef, cancel: &CancellationToken) -> Result<(), StoreError> {
        cancel.check()?;
        Err(StoreError::NotFound)
    }

    fn list_metadata(
        &self,
        _query: &MetadataQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CredentialMetadata>, StoreError> {
        cancel.check()?;
        Ok(Vec::new())
    }
}

impl<K: PlatformKeychain> PlatformKeychainAdapter<K, NoEncryptedFallback> {
    pub fn new(keychain: K) -> Self {
        Self {
            keychain,
            fallback: NoEncryptedFallback,
            ephemeral: MemoryTable::new(),
        }
    }
}

impl<K: PlatformKeychain, F: EncryptedFallback> PlatformKeychainAdapter<K, F> {
    /// Attach an already-configured encrypted fallback. Callers must not pass
    /// an implicit/unconfigured backend to weaken persistence.
    pub fn with_explicit_encrypted_fallback(keychain: K, fallback: F) -> Result<Self, StoreError> {
        if !fallback.is_explicitly_configured() {
            return Err(StoreError::PersistenceBlocked {
                reason: PersistenceBlockReason::EncryptedFallbackNotConfigured,
            });
        }
        Ok(Self {
            keychain,
            fallback,
            ephemeral: MemoryTable::new(),
        })
    }

    pub fn keychain_probe(&self) -> KeychainProbe {
        self.keychain.probe()
    }

    pub fn encrypted_fallback_configured(&self) -> bool {
        self.fallback.is_explicitly_configured()
    }
}

impl<K: PlatformKeychain, F: EncryptedFallback> CredentialStore for PlatformKeychainAdapter<K, F> {
    fn put(&self, put: CredentialPut, cancel: &CancellationToken) -> Result<SecretRef, StoreError> {
        cancel.check()?;
        match put.retention() {
            Retention::Ephemeral => self
                .ephemeral
                .put(put, CredentialBackendKind::Memory, cancel),
            Retention::Persist => {
                let probe = self.keychain.probe();
                if probe.is_available() {
                    persist_to_keychain(&self.keychain, &put, cancel)?;
                    Ok(put.secret_ref().clone())
                } else if self.fallback.is_explicitly_configured() {
                    self.fallback.put(&put, cancel)
                } else {
                    Err(StoreError::PersistenceBlocked {
                        reason: PersistenceBlockReason::KeychainUnavailable,
                    })
                }
            }
        }
    }

    fn get(
        &self,
        refer: &SecretRef,
        cancel: &CancellationToken,
    ) -> Result<SecretValue, StoreError> {
        cancel.check()?;
        match self.ephemeral.get(refer, cancel) {
            Ok(value) => return Ok(value),
            Err(StoreError::NotFound) => {}
            Err(err) => return Err(err),
        }
        if self.keychain.probe().is_available() {
            match fetch_from_keychain(&self.keychain, refer, cancel) {
                Ok(value) => return Ok(value),
                Err(StoreError::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        if self.fallback.is_explicitly_configured() {
            self.fallback.get(refer, cancel)
        } else {
            Err(StoreError::NotFound)
        }
    }

    fn delete(&self, refer: &SecretRef, cancel: &CancellationToken) -> Result<(), StoreError> {
        cancel.check()?;
        let ephemeral = match self.ephemeral.delete(refer, cancel) {
            Ok(()) => true,
            Err(StoreError::NotFound) => false,
            Err(err) => return Err(err),
        };
        let keychain = if self.keychain.probe().is_available() {
            match delete_from_keychain(&self.keychain, refer, cancel) {
                Ok(()) => true,
                Err(StoreError::NotFound) => false,
                Err(err) => return Err(err),
            }
        } else {
            false
        };
        let fallback = if self.fallback.is_explicitly_configured() {
            match self.fallback.delete(refer, cancel) {
                Ok(()) => true,
                Err(StoreError::NotFound) => false,
                Err(err) => return Err(err),
            }
        } else {
            false
        };
        if ephemeral || keychain || fallback {
            Ok(())
        } else {
            Err(StoreError::NotFound)
        }
    }

    fn list_metadata(
        &self,
        query: &MetadataQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CredentialMetadata>, StoreError> {
        cancel.check()?;
        if query.limit > MAX_METADATA_LIST {
            return Err(StoreError::BoundExceeded {
                limit: MAX_METADATA_LIST,
                requested: query.limit,
            });
        }
        let mut items = self.ephemeral.list_metadata(
            &MetadataQuery {
                provider: query.provider.clone(),
                kind: query.kind,
                limit: MAX_METADATA_LIST,
            },
            cancel,
        )?;
        if self.keychain.probe().is_available() {
            for item in self.keychain.list(cancel)? {
                let meta = CredentialMetadata {
                    refer: item.refer,
                    kind: item.kind,
                    provider: item.provider,
                    backend: CredentialBackendKind::PlatformKeychain,
                    retention: Retention::Persist,
                    byte_len: item.byte_len,
                };
                if matches_query(&meta, query) {
                    items.push(meta);
                }
            }
        }
        if self.fallback.is_explicitly_configured() {
            let fallback_items = self.fallback.list_metadata(
                &MetadataQuery {
                    provider: query.provider.clone(),
                    kind: query.kind,
                    limit: MAX_METADATA_LIST,
                },
                cancel,
            )?;
            items.extend(fallback_items);
        }
        if items.len() > query.limit {
            items.truncate(query.limit);
        }
        Ok(items)
    }
}

impl MemoryTable {
    fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<StoredCredential>>, StoreError> {
        self.records.lock().map_err(|_| StoreError::LockPoisoned)
    }

    fn put(
        &self,
        put: CredentialPut,
        backend: CredentialBackendKind,
        cancel: &CancellationToken,
    ) -> Result<SecretRef, StoreError> {
        cancel.check()?;
        if put.value.is_empty() {
            return Err(SecretError::Empty.into());
        }
        if put.value.len() > MAX_SECRET_BYTES {
            return Err(StoreError::BoundExceeded {
                limit: MAX_SECRET_BYTES,
                requested: put.value.len(),
            });
        }
        let mut records = self.lock()?;
        if let Some(index) = records
            .iter()
            .position(|stored| refs_match(&stored.refer, put.secret_ref()))
        {
            wipe_bytes(&mut records[index].bytes);
            records[index] = StoredCredential::from_put(&put, backend)?;
            return Ok(put.secret_ref().clone());
        }
        if records.len() >= MAX_METADATA_LIST {
            return Err(StoreError::BoundExceeded {
                limit: MAX_METADATA_LIST,
                requested: records.len().saturating_add(1),
            });
        }
        let refer = put.secret_ref().clone();
        records.push(StoredCredential::from_put(&put, backend)?);
        Ok(refer)
    }

    fn get(
        &self,
        refer: &SecretRef,
        cancel: &CancellationToken,
    ) -> Result<SecretValue, StoreError> {
        cancel.check()?;
        let records = self.lock()?;
        let stored = records
            .iter()
            .find(|stored| refs_match(&stored.refer, refer))
            .ok_or(StoreError::NotFound)?;
        Ok(SecretValue::from_bytes(
            stored.refer.clone(),
            stored.bytes.clone(),
        )?)
    }

    fn delete(&self, refer: &SecretRef, cancel: &CancellationToken) -> Result<(), StoreError> {
        cancel.check()?;
        let mut records = self.lock()?;
        let index = records
            .iter()
            .position(|stored| refs_match(&stored.refer, refer))
            .ok_or(StoreError::NotFound)?;
        let mut stored = records.remove(index);
        wipe_bytes(&mut stored.bytes);
        Ok(())
    }

    fn list_metadata(
        &self,
        query: &MetadataQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CredentialMetadata>, StoreError> {
        cancel.check()?;
        if query.limit > MAX_METADATA_LIST {
            return Err(StoreError::BoundExceeded {
                limit: MAX_METADATA_LIST,
                requested: query.limit,
            });
        }
        let records = self.lock()?;
        let mut items = Vec::new();
        for stored in records.iter() {
            if items.len() >= query.limit {
                break;
            }
            let meta = stored.metadata();
            if matches_query(&meta, query) {
                items.push(meta);
            }
        }
        Ok(items)
    }
}

impl Drop for MemoryTable {
    fn drop(&mut self) {
        if let Ok(mut records) = self.records.lock() {
            for stored in records.iter_mut() {
                wipe_bytes(&mut stored.bytes);
            }
            records.clear();
        }
    }
}

impl StoredCredential {
    fn from_put(put: &CredentialPut, backend: CredentialBackendKind) -> Result<Self, StoreError> {
        // Re-materialize bytes only inside Auth. Callers cannot Debug/Display them.
        let mut token = crate::secret::SecretBrokerToken::issue(
            put.secret_ref().clone(),
            crate::secret::SecretTarget::new("auth.credential_store").map_err(StoreError::from)?,
        );
        let exposed = put
            .value
            .expose(
                &mut token,
                &crate::secret::SecretTarget::new("auth.credential_store")?,
            )
            .map_err(StoreError::from)?;
        Ok(Self {
            refer: put.secret_ref().clone(),
            bytes: exposed.as_bytes().to_vec(),
            kind: put.kind,
            provider: put.provider.clone(),
            backend,
            retention: put.retention,
        })
    }

    fn metadata(&self) -> CredentialMetadata {
        CredentialMetadata {
            refer: self.refer.clone(),
            kind: self.kind,
            provider: self.provider.clone(),
            backend: self.backend,
            retention: self.retention,
            byte_len: self.bytes.len(),
        }
    }
}

fn persist_to_keychain<K: PlatformKeychain>(
    keychain: &K,
    put: &CredentialPut,
    cancel: &CancellationToken,
) -> Result<(), StoreError> {
    cancel.check()?;
    let item = KeychainItemMeta::new(put.secret_ref().clone(), put.kind, put.provider.clone())?
        .with_byte_len(put.byte_len())?;
    let mut token = crate::secret::SecretBrokerToken::issue(
        put.secret_ref().clone(),
        crate::secret::SecretTarget::new("auth.credential_store")?,
    );
    let target = crate::secret::SecretTarget::new("auth.credential_store")?;
    let exposed = put.value.expose(&mut token, &target)?;
    keychain.put(&item, exposed.as_bytes(), cancel)
}

fn fetch_from_keychain<K: PlatformKeychain>(
    keychain: &K,
    refer: &SecretRef,
    cancel: &CancellationToken,
) -> Result<SecretValue, StoreError> {
    let item = KeychainItemMeta::new(refer.clone(), CredentialKind::Generic, None)?;
    let bytes = keychain.get(&item, cancel)?;
    Ok(SecretValue::from_bytes(refer.clone(), bytes)?)
}

fn delete_from_keychain<K: PlatformKeychain>(
    keychain: &K,
    refer: &SecretRef,
    cancel: &CancellationToken,
) -> Result<(), StoreError> {
    let item = KeychainItemMeta::new(refer.clone(), CredentialKind::Generic, None)?;
    keychain.delete(&item, cancel)
}

fn refs_match(stored: &SecretRef, query: &SecretRef) -> bool {
    stored.matches(query)
}

fn matches_query(meta: &CredentialMetadata, query: &MetadataQuery) -> bool {
    if let Some(provider) = query.provider.as_deref()
        && meta.provider.as_deref() != Some(provider)
    {
        return false;
    }
    if let Some(kind) = query.kind
        && meta.kind != kind
    {
        return false;
    }
    true
}

fn parse_provider(provider: &str) -> Result<String, StoreError> {
    if provider.is_empty() {
        return Err(StoreError::InvalidProvider);
    }
    if provider.len() > MAX_PROVIDER_BYTES {
        return Err(StoreError::BoundExceeded {
            limit: MAX_PROVIDER_BYTES,
            requested: provider.len(),
        });
    }
    if !provider
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        return Err(StoreError::InvalidProvider);
    }
    Ok(provider.to_ascii_lowercase())
}

fn keychain_account(refer: &SecretRef) -> Result<String, StoreError> {
    let account = refer
        .id()
        .or_else(|| refer.alias())
        .ok_or(StoreError::InvalidHandle)?;
    if account.len() > MAX_KEYCHAIN_ACCOUNT_BYTES {
        return Err(StoreError::BoundExceeded {
            limit: MAX_KEYCHAIN_ACCOUNT_BYTES,
            requested: account.len(),
        });
    }
    if account.len() > MAX_SECRET_REF_ALIAS_BYTES && refer.alias().is_some() && refer.id().is_none()
    {
        return Err(StoreError::BoundExceeded {
            limit: MAX_SECRET_REF_ALIAS_BYTES,
            requested: account.len(),
        });
    }
    Ok(account.to_string())
}

fn wipe_bytes(buf: &mut Vec<u8>) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    compiler_fence(Ordering::SeqCst);
    buf.clear();
}

impl Debug for CredentialMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialMetadata")
            .field("redacted", &true)
            .field("kind", &self.kind)
            .field("provider", &self.provider)
            .field("backend", &self.backend)
            .field("retention", &self.retention)
            .field("byte_len", &self.byte_len)
            .finish()
    }
}

impl Display for CredentialMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CredentialMetadata(<redacted-handle>, kind={:?}, backend={:?}, byte_len={})",
            self.kind, self.backend, self.byte_len
        )
    }
}

impl Debug for CredentialPut {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialPut")
            .field("redacted", &true)
            .field("kind", &self.kind)
            .field("provider", &self.provider)
            .field("retention", &self.retention)
            .field("byte_len", &self.value.len())
            .finish()
    }
}

impl Debug for KeychainItemMeta {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeychainItemMeta")
            .field("redacted", &true)
            .field("service", &self.service)
            .field("kind", &self.kind)
            .field("provider", &self.provider)
            .finish()
    }
}

impl Serialize for CredentialMetadata {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let field_count = 8 + usize::from(self.provider.is_some());
        let mut state = serializer.serialize_struct("CredentialMetadata", field_count)?;
        state.serialize_field("schema", CREDENTIAL_METADATA_SCHEMA)?;
        state.serialize_field("schema_version", &SCHEMA_VERSION)?;
        state.serialize_field("ref", &self.refer)?;
        state.serialize_field("kind", &self.kind)?;
        if let Some(provider) = &self.provider {
            state.serialize_field("provider", provider)?;
        }
        state.serialize_field("backend", &self.backend)?;
        state.serialize_field("retention", &self.retention)?;
        state.serialize_field("byte_len", &self.byte_len)?;
        state.serialize_field("redacted", &true)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CredentialMetadata {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            schema: Option<String>,
            #[serde(default)]
            schema_version: Option<u32>,
            #[serde(rename = "ref")]
            refer: SecretRef,
            kind: CredentialKind,
            #[serde(default)]
            provider: Option<String>,
            backend: CredentialBackendKind,
            retention: Retention,
            byte_len: usize,
            #[serde(default)]
            redacted: Option<bool>,
            #[serde(default)]
            value: Option<IgnoredAny>,
            #[serde(default)]
            secret: Option<IgnoredAny>,
            #[serde(default)]
            password: Option<IgnoredAny>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.value.is_some() || wire.secret.is_some() || wire.password.is_some() {
            return Err(D::Error::custom(StoreError::InvalidMetadata.to_string()));
        }
        if let Some(schema) = wire.schema.as_deref()
            && schema != CREDENTIAL_METADATA_SCHEMA
        {
            return Err(D::Error::custom("unsupported CredentialMetadata schema"));
        }
        if let Some(version) = wire.schema_version
            && version != SCHEMA_VERSION
        {
            return Err(D::Error::custom(
                "unsupported CredentialMetadata schema_version",
            ));
        }
        let _ = wire.redacted;
        if wire.byte_len > MAX_SECRET_BYTES {
            return Err(D::Error::custom(StoreError::BoundExceeded {
                limit: MAX_SECRET_BYTES,
                requested: wire.byte_len,
            }));
        }
        if let Some(provider) = wire.provider.as_deref() {
            parse_provider(provider).map_err(D::Error::custom)?;
        }
        Ok(Self {
            refer: wire.refer,
            kind: wire.kind,
            provider: wire.provider,
            backend: wire.backend,
            retention: wire.retention,
            byte_len: wire.byte_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::{SecretBrokerToken, SecretTarget};

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-9f3c2a";
    const REF_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const REF_ALIAS: &str = "env:CANARY";

    fn sample_ref() -> SecretRef {
        SecretRef::from_id_and_alias(REF_ID, REF_ALIAS).expect("valid test SecretRef")
    }

    fn sample_value() -> SecretValue {
        SecretValue::from_str(sample_ref(), CANARY).expect("valid test SecretValue")
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn expose_bytes(value: &SecretValue) -> Vec<u8> {
        let target = SecretTarget::new("auth.credential_store").expect("target");
        let mut token = SecretBrokerToken::issue(value.secret_ref().clone(), target.clone());
        value
            .expose(&mut token, &target)
            .expect("authorized expose")
            .as_bytes()
            .to_vec()
    }

    fn expose_utf8(value: &SecretValue) -> String {
        String::from_utf8(expose_bytes(value)).expect("utf8")
    }

    fn assert_no_canary(label: &str, rendered: &str) {
        assert!(
            !rendered.contains(CANARY),
            "{label} leaked secret plaintext: {rendered}"
        );
        assert!(
            !rendered.contains("PLAINTEXT-do-not-leak"),
            "{label} leaked secret substring: {rendered}"
        );
    }

    struct FakeKeychain {
        available: AtomicBool,
        table: MemoryTable,
    }

    impl FakeKeychain {
        fn available() -> Self {
            Self {
                available: AtomicBool::new(true),
                table: MemoryTable::new(),
            }
        }

        fn unavailable() -> Self {
            Self {
                available: AtomicBool::new(false),
                table: MemoryTable::new(),
            }
        }
    }

    impl PlatformKeychain for FakeKeychain {
        fn probe(&self) -> KeychainProbe {
            if self.available.load(Ordering::SeqCst) {
                KeychainProbe::available(platform_keychain_kind())
            } else {
                KeychainProbe::unavailable(platform_keychain_kind())
            }
        }

        fn put(
            &self,
            item: &KeychainItemMeta,
            secret: &[u8],
            cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            cancel.check()?;
            if !self.probe().is_available() {
                return Err(StoreError::PersistenceBlocked {
                    reason: PersistenceBlockReason::KeychainUnavailable,
                });
            }
            let value = SecretValue::from_bytes(item.refer.clone(), secret.to_vec())?;
            let mut put = CredentialPut::persist(value, item.kind);
            put.provider = item.provider.clone();
            self.table
                .put(put, CredentialBackendKind::PlatformKeychain, cancel)?;
            Ok(())
        }

        fn get(
            &self,
            item: &KeychainItemMeta,
            cancel: &CancellationToken,
        ) -> Result<Vec<u8>, StoreError> {
            cancel.check()?;
            let value = self.table.get(&item.refer, cancel)?;
            Ok(expose_bytes(&value))
        }

        fn delete(
            &self,
            item: &KeychainItemMeta,
            cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            self.table.delete(&item.refer, cancel)
        }

        fn list(&self, cancel: &CancellationToken) -> Result<Vec<KeychainItemMeta>, StoreError> {
            let metas = self.table.list_metadata(&MetadataQuery::new(), cancel)?;
            metas
                .into_iter()
                .map(|meta| {
                    KeychainItemMeta::new(meta.refer, meta.kind, meta.provider)
                        .and_then(|item| item.with_byte_len(meta.byte_len))
                })
                .collect()
        }
    }

    struct ExplicitMemoryFallback {
        configured: bool,
        table: InMemoryCredentialStore,
    }

    impl ExplicitMemoryFallback {
        fn configured() -> Self {
            Self {
                configured: true,
                table: InMemoryCredentialStore::new(),
            }
        }

        fn unconfigured() -> Self {
            Self {
                configured: false,
                table: InMemoryCredentialStore::new(),
            }
        }
    }

    impl EncryptedFallback for ExplicitMemoryFallback {
        fn is_explicitly_configured(&self) -> bool {
            self.configured
        }

        fn put(
            &self,
            put: &CredentialPut,
            cancel: &CancellationToken,
        ) -> Result<SecretRef, StoreError> {
            if !self.configured {
                return Err(StoreError::PersistenceBlocked {
                    reason: PersistenceBlockReason::EncryptedFallbackNotConfigured,
                });
            }
            let value = SecretValue::from_bytes(
                put.secret_ref().clone(),
                expose_utf8(&put.value).into_bytes(),
            )?;
            let mut next = CredentialPut::persist(value, put.kind);
            next.provider = put.provider.clone();
            self.table.put(next, cancel)
        }

        fn get(
            &self,
            refer: &SecretRef,
            cancel: &CancellationToken,
        ) -> Result<SecretValue, StoreError> {
            if !self.configured {
                return Err(StoreError::NotFound);
            }
            self.table.get(refer, cancel)
        }

        fn delete(&self, refer: &SecretRef, cancel: &CancellationToken) -> Result<(), StoreError> {
            if !self.configured {
                return Err(StoreError::NotFound);
            }
            self.table.delete(refer, cancel)
        }

        fn list_metadata(
            &self,
            query: &MetadataQuery,
            cancel: &CancellationToken,
        ) -> Result<Vec<CredentialMetadata>, StoreError> {
            if !self.configured {
                return Ok(Vec::new());
            }
            self.table.list_metadata(query, cancel)
        }
    }

    #[test]
    fn in_memory_provider_put_get_delete_list_round_trip() {
        let store = InMemoryCredentialStore::new();
        let put = CredentialPut::persist(sample_value(), CredentialKind::ProviderApiKey)
            .with_provider("openai")
            .expect("provider");
        let handle = store.put(put, &live()).expect("put");
        assert_eq!(handle, sample_ref());

        let fetched = store.get(&handle, &live()).expect("get");
        assert_eq!(expose_utf8(&fetched), CANARY);
        assert_no_canary("get Debug", &format!("{fetched:?}"));
        assert_no_canary("get Display", &format!("{fetched}"));

        let listed = store
            .list_metadata(&MetadataQuery::new(), &live())
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].secret_ref(), &handle);
        assert_eq!(listed[0].kind(), CredentialKind::ProviderApiKey);
        assert_eq!(listed[0].provider(), Some("openai"));
        assert_eq!(listed[0].byte_len(), CANARY.len());
        assert_eq!(listed[0].backend(), CredentialBackendKind::Memory);

        store.delete(&handle, &live()).expect("delete");
        assert_eq!(
            store.get(&handle, &live()).expect_err("deleted"),
            StoreError::NotFound
        );
        assert!(
            store
                .list_metadata(&MetadataQuery::new(), &live())
                .expect("empty list")
                .is_empty()
        );
    }

    #[test]
    fn metadata_api_cannot_return_secret_material() {
        let store = InMemoryCredentialStore::new();
        store
            .put(
                CredentialPut::persist(sample_value(), CredentialKind::Generic),
                &live(),
            )
            .expect("put");
        let listed = store
            .list_metadata(&MetadataQuery::new(), &live())
            .expect("list");
        let json = serde_json::to_string(&listed[0]).expect("serialize metadata");
        let debug = format!("{:?}", listed[0]);
        let display = format!("{}", listed[0]);

        assert!(json.contains("\"redacted\":true"));
        assert!(!json.contains("\"value\""));
        assert!(!json.contains("\"secret\""));
        assert!(!json.contains("\"password\""));
        assert_eq!(
            json,
            "{\"schema\":\"rapidlm.auth.credential_metadata\",\"schema_version\":1,\"ref\":{\"schema\":\"rapidlm.auth.secret_ref\",\"schema_version\":1,\"id\":\"01234567-89ab-cdef-0123-456789abcdef\",\"alias\":\"env:CANARY\"},\"kind\":\"generic\",\"backend\":\"memory\",\"retention\":\"persist\",\"byte_len\":42,\"redacted\":true}"
        );
        assert_no_canary("metadata json", &json);
        assert_no_canary("metadata Debug", &debug);
        assert_no_canary("metadata Display", &display);
    }

    #[test]
    fn metadata_deserialize_rejects_secret_fields() {
        let leaked = format!(
            "{{\"schema\":\"rapidlm.auth.credential_metadata\",\"schema_version\":1,\"ref\":{{\"alias\":\"env:CANARY\"}},\"kind\":\"generic\",\"backend\":\"memory\",\"retention\":\"persist\",\"byte_len\":42,\"value\":\"{CANARY}\"}}"
        );
        let err = serde_json::from_str::<CredentialMetadata>(&leaked).expect_err("must reject");
        assert!(err.to_string().contains("credential metadata is invalid"));
        assert_no_canary("metadata deserialize error", &err.to_string());
    }

    #[test]
    fn get_by_alias_or_id_resolves_handle() {
        let store = InMemoryCredentialStore::new();
        store
            .put(
                CredentialPut::persist(sample_value(), CredentialKind::Generic),
                &live(),
            )
            .expect("put");
        let by_alias = store
            .get(&SecretRef::from_alias(REF_ALIAS).expect("alias"), &live())
            .expect("alias get");
        let by_id = store
            .get(&SecretRef::from_id(REF_ID).expect("id"), &live())
            .expect("id get");
        assert_eq!(expose_utf8(&by_alias), CANARY);
        assert_eq!(expose_utf8(&by_id), CANARY);
    }

    #[test]
    fn cancelled_operations_fail_closed() {
        let store = InMemoryCredentialStore::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = store
            .put(
                CredentialPut::persist(sample_value(), CredentialKind::Generic),
                &cancel,
            )
            .expect_err("cancelled put");
        assert_eq!(err, StoreError::Cancelled);
        assert_eq!(err.code(), "auth.cancelled");
        assert!(!err.retryable());
        assert_eq!(
            store
                .get(&sample_ref(), &cancel)
                .expect_err("cancelled get"),
            StoreError::Cancelled
        );
    }

    #[test]
    fn list_limit_and_provider_filter_are_bounded() {
        let store = InMemoryCredentialStore::new();
        store
            .put(
                CredentialPut::persist(sample_value(), CredentialKind::ProviderApiKey)
                    .with_provider("openai")
                    .expect("provider"),
                &live(),
            )
            .expect("put");
        let other = SecretValue::from_str(
            SecretRef::from_alias("env:OTHER").expect("other"),
            "other-secret-value",
        )
        .expect("value");
        store
            .put(
                CredentialPut::persist(other, CredentialKind::DaemonToken)
                    .with_provider("daemon")
                    .expect("provider"),
                &live(),
            )
            .expect("put other");

        let filtered = store
            .list_metadata(
                &MetadataQuery::new()
                    .with_provider("openai")
                    .expect("query")
                    .with_kind(CredentialKind::ProviderApiKey),
                &live(),
            )
            .expect("filter");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider(), Some("openai"));

        let limited = store
            .list_metadata(
                &MetadataQuery::new().with_limit(0).expect("limit 0"),
                &live(),
            )
            .expect("limit");
        assert!(limited.is_empty());

        let err = MetadataQuery::new()
            .with_limit(MAX_METADATA_LIST + 1)
            .expect_err("oversize limit");
        assert_eq!(
            err,
            StoreError::BoundExceeded {
                limit: MAX_METADATA_LIST,
                requested: MAX_METADATA_LIST + 1,
            }
        );
    }

    #[test]
    fn unimplemented_keychain_blocks_persist_without_fallback() {
        let store = PlatformKeychainAdapter::new(UnimplementedPlatformKeychain);
        assert!(!store.keychain_probe().is_available());
        assert!(!store.encrypted_fallback_configured());
        let err = store
            .put(
                CredentialPut::persist(sample_value(), CredentialKind::Generic),
                &live(),
            )
            .expect_err("persist must block");
        assert_eq!(
            err,
            StoreError::PersistenceBlocked {
                reason: PersistenceBlockReason::KeychainUnavailable,
            }
        );
        assert_eq!(err.code(), "auth.keychain_unavailable");
        assert!(!err.retryable());
        assert_no_canary("persist error", &err.to_string());
    }

    #[test]
    fn ephemeral_put_succeeds_when_keychain_unavailable() {
        let store = PlatformKeychainAdapter::new(UnimplementedPlatformKeychain);
        let handle = store
            .put(
                CredentialPut::ephemeral(sample_value(), CredentialKind::Generic),
                &live(),
            )
            .expect("ephemeral put");
        let fetched = store.get(&handle, &live()).expect("ephemeral get");
        assert_eq!(expose_utf8(&fetched), CANARY);
        let listed = store
            .list_metadata(&MetadataQuery::new(), &live())
            .expect("list");
        assert_eq!(listed[0].retention(), Retention::Ephemeral);
        assert_eq!(listed[0].backend(), CredentialBackendKind::Memory);
        assert_no_canary(
            "ephemeral metadata",
            &serde_json::to_string(&listed[0]).unwrap(),
        );
    }

    #[test]
    fn adapter_does_not_use_unconfigured_fallback() {
        let err = match PlatformKeychainAdapter::with_explicit_encrypted_fallback(
            FakeKeychain::unavailable(),
            ExplicitMemoryFallback::unconfigured(),
        ) {
            Ok(_) => panic!("unconfigured fallback rejected"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            StoreError::PersistenceBlocked {
                reason: PersistenceBlockReason::EncryptedFallbackNotConfigured,
            }
        );
    }

    #[test]
    fn explicit_fallback_is_used_only_when_keychain_unavailable() {
        let store = PlatformKeychainAdapter::with_explicit_encrypted_fallback(
            FakeKeychain::unavailable(),
            ExplicitMemoryFallback::configured(),
        )
        .expect("explicit fallback");
        let handle = store
            .put(
                CredentialPut::persist(sample_value(), CredentialKind::OauthRefresh),
                &live(),
            )
            .expect("fallback persist");
        assert_eq!(
            expose_utf8(&store.get(&handle, &live()).expect("get")),
            CANARY
        );
        store.delete(&handle, &live()).expect("delete");
        assert_eq!(
            store.get(&handle, &live()).expect_err("gone"),
            StoreError::NotFound
        );
    }

    #[test]
    fn available_keychain_does_not_degrade_to_fallback_on_put() {
        let store = PlatformKeychainAdapter::with_explicit_encrypted_fallback(
            FakeKeychain::available(),
            ExplicitMemoryFallback::configured(),
        )
        .expect("adapter");
        let handle = store
            .put(
                CredentialPut::persist(sample_value(), CredentialKind::Generic),
                &live(),
            )
            .expect("keychain persist");
        let listed = store
            .list_metadata(&MetadataQuery::new(), &live())
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].backend(), CredentialBackendKind::PlatformKeychain);
        assert_eq!(listed[0].byte_len(), CANARY.len());
        assert_eq!(
            expose_utf8(&store.get(&handle, &live()).expect("get")),
            CANARY
        );
    }

    #[test]
    fn unavailable_keychain_put_does_not_write_secret_to_metadata() {
        let store = PlatformKeychainAdapter::new(FakeKeychain::unavailable());
        let _ = store.put(
            CredentialPut::persist(sample_value(), CredentialKind::Generic),
            &live(),
        );
        let listed = store
            .list_metadata(&MetadataQuery::new(), &live())
            .expect("list");
        assert!(listed.is_empty());
    }

    #[test]
    fn error_and_put_debug_omit_secret_material() {
        let put = CredentialPut::persist(sample_value(), CredentialKind::Generic);
        assert_no_canary("CredentialPut Debug", &format!("{put:?}"));
        for err in [
            StoreError::Cancelled,
            StoreError::NotFound,
            StoreError::PersistenceBlocked {
                reason: PersistenceBlockReason::KeychainUnavailable,
            },
            StoreError::InvalidMetadata,
        ] {
            assert_no_canary("StoreError", &format!("{err:?} {err} {}", err.code()));
        }
    }
}
