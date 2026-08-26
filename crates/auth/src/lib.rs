#![forbid(unsafe_code)]

pub mod broker;
pub mod env_identity;
pub mod file_keychain;
pub mod os_keychain;
pub mod local_daemon;
pub mod mtls;
pub mod secret;
pub mod store;

pub use broker::{BoundSecret, ScopedSecret, SecretBroker};
pub use env_identity::{CredentialCacheKey, EnvIdentity};
pub use file_keychain::FileKeychain;
pub use local_daemon::{
    AuthChallenge, AuthProof, AuthorizedSessionApi, ClientGrant, DaemonAuth, DaemonAuthError,
    DaemonTokenHandle, LocalDaemonClient, OsUserBinding, SessionApiKind, TOKEN_DIR_MODE,
    TOKEN_FILE_MODE, TOKEN_FILE_NAME,
};
pub use mtls::{
    AuthenticatedController, AuthenticatedWorker, CertFingerprint, MtlsError, MtlsTrust,
    PresentedCertificate, WorkerClaims, authenticate_controller, authenticate_worker,
};
pub use secret::{
    ExposedSecret, MAX_SECRET_BYTES, MAX_SECRET_REF_ALIAS_BYTES, MAX_SECRET_REF_ID_BYTES,
    MAX_SECRET_TARGET_BYTES, SecretAwareValue, SecretBrokerToken, SecretError, SecretRef,
    SecretTarget, SecretValue,
};
pub use store::{
    CancellationToken, CredentialBackendKind, CredentialKind, CredentialMetadata, CredentialPut,
    CredentialStore, EncryptedFallback, InMemoryCredentialStore, KEYCHAIN_SERVICE,
    KeychainItemMeta, KeychainProbe, MAX_KEYCHAIN_ACCOUNT_BYTES, MAX_METADATA_LIST,
    MAX_PROVIDER_BYTES, MetadataQuery, NoEncryptedFallback, PersistenceBlockReason,
    PlatformKeychain, PlatformKeychainAdapter, PlatformKeychainKind, Retention, StoreError,
    UnimplementedPlatformKeychain, platform_keychain_kind,
};
