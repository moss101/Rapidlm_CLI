//! Durable, platform-neutral file-backed `PlatformKeychain`.
//!
//! Persists each credential as an owner-only file pair under a root directory:
//! `<sha256(account|service)>.meta.json` (handle metadata, no secret bytes) and
//! `<sha256(account|service)>.bin` (the secret). This is the filesystem-backed
//! durable store used where no OS keychain is available. The public abstraction
//! stays the platform-neutral [`PlatformKeychain`]; this is one implementation
//! of it, not a second credential authority.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::secret::SecretRef;
use crate::store::{
    CredentialKind, KeychainItemMeta, KeychainProbe, PlatformKeychain, PlatformKeychainKind,
    StoreError,
};

const META_SCHEMA: &str = "rapidlm.auth.file_keychain_item";
const META_SUFFIX: &str = ".meta.json";
const SECRET_SUFFIX: &str = ".bin";

/// Durable file-backed credential store.
pub struct FileKeychain {
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    schema: String,
    schema_version: u32,
    secret_ref: SecretRef,
    kind: CredentialKind,
    provider: Option<String>,
}

impl FileKeychain {
    /// Open a store rooted at `root`. The directory is created on first write.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn meta_path(&self, item: &KeychainItemMeta) -> PathBuf {
        self.root.join(format!("{}{}", item_key(item), META_SUFFIX))
    }

    fn secret_path(&self, item: &KeychainItemMeta) -> PathBuf {
        self.root
            .join(format!("{}{}", item_key(item), SECRET_SUFFIX))
    }
}

impl PlatformKeychain for FileKeychain {
    fn probe(&self) -> KeychainProbe {
        let available = if self.root.exists() {
            fs::metadata(&self.root)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            // A filesystem that is writable at this path is treated as supported;
            // an unwritable root fails closed at the first put/get.
            true
        };
        if available {
            KeychainProbe::available(PlatformKeychainKind::File)
        } else {
            KeychainProbe::unavailable(PlatformKeychainKind::File)
        }
    }

    fn put(
        &self,
        item: &KeychainItemMeta,
        secret: &[u8],
        cancel: &crate::store::CancellationToken,
    ) -> Result<(), StoreError> {
        cancel.check()?;
        fs::create_dir_all(&self.root).map_err(|_| StoreError::PersistenceBlocked {
            reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
        })?;
        let meta = Meta {
            schema: META_SCHEMA.to_owned(),
            schema_version: 1,
            secret_ref: item.secret_ref().clone(),
            kind: item.kind(),
            provider: item.provider().map(str::to_owned),
        };
        let meta_bytes = serde_json::to_vec(&meta).map_err(|_| StoreError::InvalidMetadata)?;
        write_owner_only(&self.meta_path(item), &meta_bytes)?;
        write_owner_only(&self.secret_path(item), secret)?;
        Ok(())
    }

    fn get(
        &self,
        item: &KeychainItemMeta,
        cancel: &crate::store::CancellationToken,
    ) -> Result<Vec<u8>, StoreError> {
        cancel.check()?;
        fs::read(self.secret_path(item)).map_err(|_| StoreError::NotFound)
    }

    fn delete(
        &self,
        item: &KeychainItemMeta,
        cancel: &crate::store::CancellationToken,
    ) -> Result<(), StoreError> {
        cancel.check()?;
        let meta_hit = fs::remove_file(self.meta_path(item)).is_ok();
        let secret_hit = fs::remove_file(self.secret_path(item)).is_ok();
        if meta_hit || secret_hit {
            Ok(())
        } else {
            Err(StoreError::NotFound)
        }
    }

    fn list(
        &self,
        cancel: &crate::store::CancellationToken,
    ) -> Result<Vec<KeychainItemMeta>, StoreError> {
        cancel.check()?;
        let entries = fs::read_dir(&self.root).map_err(|_| StoreError::NotFound)?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.ends_with(META_SUFFIX) {
                continue;
            }
            let bytes = match fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let Ok(meta) = serde_json::from_slice::<Meta>(&bytes) else {
                continue;
            };
            if meta.schema != META_SCHEMA {
                continue;
            }
            let mut item = KeychainItemMeta::new(meta.secret_ref, meta.kind, meta.provider)
                .map_err(|_| StoreError::InvalidMetadata)?;
            let secret_path = self.root.join(format!(
                "{}{}",
                name.strip_suffix(META_SUFFIX).unwrap_or(name),
                SECRET_SUFFIX
            ));
            if let Ok(meta2) = fs::metadata(&secret_path) {
                item = item.with_byte_len(meta2.len() as usize).map_err(|_| {
                    StoreError::BoundExceeded {
                        limit: 0,
                        requested: meta2.len() as usize,
                    }
                })?;
            }
            out.push(item);
        }
        out.sort_by_key(|item| item.account().to_owned());
        Ok(out)
    }
}

fn item_key(item: &KeychainItemMeta) -> String {
    let mut hasher = Sha256::new();
    hasher.update(item.service().as_bytes());
    hasher.update(b"|");
    hasher.update(item.account().as_bytes());
    hex(&hasher.finalize())
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let blocked = || StoreError::PersistenceBlocked {
        reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
    };
    // Open with the owner-only mode already set, rather than `fs::write`
    // (which creates the file at the process's default, umask-derived mode
    // — typically world/group-readable) followed by a chmod: that ordering
    // left a real window where a fresh secret file was created and fully
    // written before ever being restricted, readable by any local reader
    // that sampled the directory in between. `create` (not `create_new`)
    // still allows overwriting an existing item's stored secret.
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|_| blocked())?;
    file.write_all(bytes).map_err(|_| blocked())?;
    // Defense in depth: `mode()` above only applies when this call actually
    // creates the file — if it already existed (e.g. from an older, less
    // careful write, or a platform where open-time mode isn't honored),
    // tighten it explicitly too rather than trust it was already correct.
    restrict_permissions(path);
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {
    // Best-effort on non-Unix; secrets remain behind the filesystem ACL.
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::{SecretRef, SecretValue};
    use crate::store::{
        CancellationToken, CredentialPut, CredentialStore, PlatformKeychainAdapter, Retention,
    };

    fn refer() -> SecretRef {
        SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("refer")
    }

    fn item() -> KeychainItemMeta {
        KeychainItemMeta::new(
            refer(),
            CredentialKind::ProviderApiKey,
            Some("anthropic".to_owned()),
        )
        .expect("item")
    }

    fn scratch_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        std::env::temp_dir()
            .join("rapidlm-filekeychain")
            .join(unique)
    }

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn put_get_round_trips_and_reports_available() {
        let fx = Fixture(scratch_dir());
        let keychain = FileKeychain::new(&fx.0);
        assert!(keychain.probe().is_available());
        let cancel = CancellationToken::new();
        keychain
            .put(&item(), b"sk-super-secret", &cancel)
            .expect("put");

        let bytes = keychain.get(&item(), &cancel).expect("get");
        assert_eq!(bytes, b"sk-super-secret");

        // A second FileKeychain over the same root reads the same durable bytes,
        // proving the backend survives the process/session boundary.
        let reopened = FileKeychain::new(&fx.0);
        assert_eq!(
            reopened.get(&item(), &cancel).expect("reopened"),
            b"sk-super-secret"
        );

        // Re-putting the same item (rotation) must overwrite, not fail and
        // not leave stale trailing bytes behind if the new secret is
        // shorter — `write_owner_only` opens with `create` + `truncate`,
        // not `create_new`, specifically so this keeps working.
        keychain.put(&item(), b"short", &cancel).expect("re-put");
        assert_eq!(
            keychain.get(&item(), &cancel).expect("get after re-put"),
            b"short"
        );
    }

    #[test]
    fn missing_secret_is_not_found_and_does_not_invent_bytes() {
        let fx = Fixture(scratch_dir());
        let keychain = FileKeychain::new(&fx.0);
        let cancel = CancellationToken::new();
        assert_eq!(keychain.get(&item(), &cancel), Err(StoreError::NotFound));
    }

    #[test]
    fn delete_removes_and_list_reports_metadata_never_secret() {
        let fx = Fixture(scratch_dir());
        let keychain = FileKeychain::new(&fx.0);
        let cancel = CancellationToken::new();
        keychain
            .put(&item(), b"sk-durable-secret", &cancel)
            .expect("put");

        let listed = keychain.list(&cancel).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].secret_ref(), &refer());
        assert_eq!(listed[0].kind(), CredentialKind::ProviderApiKey);
        assert_eq!(
            listed[0].provider().map(str::to_owned),
            Some("anthropic".to_owned())
        );

        // Metadata files never carry the secret bytes: the only file containing
        // "sk-durable-secret" is the separate .bin file.
        for entry in fs::read_dir(&fx.0).expect("dir").flatten() {
            let path = entry.path();
            let content = fs::read(&path).unwrap_or_default();
            if path.extension().and_then(|e| e.to_str()) == Some("bin") {
                assert_eq!(content, b"sk-durable-secret");
            } else {
                assert!(!content.windows(16).any(|w| w == b"sk-durable-secret"));
            }
        }

        keychain.delete(&item(), &cancel).expect("delete");
        assert!(keychain.list(&cancel).expect("list again").is_empty());
        assert_eq!(keychain.get(&item(), &cancel), Err(StoreError::NotFound));
    }

    #[test]
    fn meta_and_secret_files_are_owner_only_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let fx = Fixture(scratch_dir());
            let keychain = FileKeychain::new(&fx.0);
            let cancel = CancellationToken::new();
            keychain.put(&item(), b"secret", &cancel).expect("put");
            let files = std::fs::read_dir(&fx.0).expect("read_dir");
            let mut checked = 0;
            for entry in files.flatten() {
                let mode = entry.metadata().expect("meta").permissions().mode();
                assert_eq!(
                    mode & 0o777,
                    0o600,
                    "{} not owner-only",
                    entry.path().display()
                );
                checked += 1;
            }
            assert!(checked >= 2, "expected meta + secret files");
        }
    }

    #[test]
    fn meta_schema_rejects_unknown_fields() {
        let unknown = br#"{"schema":"rapidlm.auth.file_keychain_item","schema_version":1,"secret_ref":{"schema":"rapidlm.auth.secret_ref","schema_version":1,"id":"018f3c8a-7e2b-7a10-8c4d-0123456789ab"},"kind":"provider_api_key","provider":"x","extra":true}"#;
        assert!(serde_json::from_slice::<Meta>(unknown).is_err());
    }

    #[test]
    fn adapter_with_file_keychain_is_a_durable_credential_store() {
        let fx = Fixture(scratch_dir());
        let cancel = CancellationToken::new();
        let store = PlatformKeychainAdapter::new(FileKeychain::new(&fx.0));
        assert!(store.keychain_probe().is_available());

        let put = CredentialPut::persist(
            SecretValue::from_bytes(refer(), b"sk-durable-key".to_vec()).expect("value"),
            CredentialKind::ProviderApiKey,
        );
        let handle = store.put(put, &cancel).expect("persist");
        assert_eq!(handle, refer());

        let got = store.get(&refer(), &cancel).expect("get");
        assert_eq!(got.len(), b"sk-durable-key".len());
        assert_eq!(got.secret_ref(), &refer());

        // Durable across a fresh adapter over the same root.
        let reopened = PlatformKeychainAdapter::new(FileKeychain::new(&fx.0));
        let again = reopened.get(&refer(), &cancel).expect("reopened get");
        assert_eq!(again.len(), b"sk-durable-key".len());

        // Ephemeral retention must NOT be written to the durable store.
        let ephemeral_ref =
            SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ac").expect("eph ref");
        let e_store = PlatformKeychainAdapter::new(FileKeychain::new(&fx.0));
        let retention = Retention::Ephemeral;
        let e_put = CredentialPut::ephemeral(
            SecretValue::from_bytes(ephemeral_ref.clone(), b"sk-ephemeral".to_vec())
                .expect("value"),
            CredentialKind::Generic,
        );
        e_store.put(e_put, &cancel).expect("ephemeral put");
        assert!(e_store.get(&ephemeral_ref, &cancel).is_ok());
        let reopened_e = PlatformKeychainAdapter::new(FileKeychain::new(&fx.0));
        assert!(reopened_e.get(&ephemeral_ref, &cancel).is_err());
        assert_eq!(retention, Retention::Ephemeral);
    }
}
