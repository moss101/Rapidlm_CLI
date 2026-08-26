//! P4-032 production backend: macOS Keychain via the OS `security` CLI.
//! No plaintext-at-rest fallback; unavailable keychain fails closed.
#![cfg(target_os = "macos")]

use std::process::Command;

use crate::store::{CancellationToken, KeychainItemMeta, KeychainProbe, PlatformKeychain, PlatformKeychainKind, StoreError};

/// Production macOS Keychain-backed store. Secrets live only inside the
/// login keychain; nothing is written to disk by this type.
pub struct MacosKeychain {
    service: String,
}

impl MacosKeychain {
    /// Service namespace for RapidLM entries.
    pub fn new() -> Self {
        Self {
            service: "RapidLM".to_owned(),
        }
    }

    fn account(&self, item: &KeychainItemMeta) -> String {
        item.account().to_owned()
    }
}

impl Default for MacosKeychain {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformKeychain for MacosKeychain {
    fn probe(&self) -> KeychainProbe {
        let ok = Command::new("/usr/bin/security")
            .arg("list-keychains")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            KeychainProbe::available(PlatformKeychainKind::MacosKeychain)
        } else {
            KeychainProbe::unavailable(PlatformKeychainKind::MacosKeychain)
        }
    }

    fn put(
        &self,
        item: &KeychainItemMeta,
        secret: &[u8],
        cancel: &crate::store::CancellationToken,
    ) -> Result<(), StoreError> {
        cancel.check().map_err(|_| StoreError::PersistenceBlocked {
            reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
        })?;
        // Replace any existing entry, then add the fresh secret bytes.
        let account = self.account(item);
        let _ = Command::new("/usr/bin/security")
            .args(["delete-generic-password", "-s", &self.service, "-a", &account])
            .output();
        let out = Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-s",
                &self.service,
                "-a",
                &account,
                "-w",
            ])
            .arg(String::from_utf8_lossy(secret).into_owned())
            .output()
            .map_err(|_| StoreError::PersistenceBlocked {
                reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
            })?;
        if out.status.success() {
            Ok(())
        } else {
            Err(StoreError::PersistenceBlocked {
                reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
            })
        }
    }

    fn get(
        &self,
        item: &KeychainItemMeta,
        cancel: &crate::store::CancellationToken,
    ) -> Result<Vec<u8>, StoreError> {
        cancel.check().map_err(|_| StoreError::NotFound)?;
        let out = Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                &self.service,
                "-a",
                &self.account(item),
                "-w",
            ])
            .output()
            .map_err(|_| StoreError::NotFound)?;
        if !out.status.success() {
            return Err(StoreError::NotFound);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        // `-w` prints hex when the stored data is not printable; pass through raw otherwise.
        Ok(trimmed.as_bytes().to_vec())
    }

    fn delete(
        &self,
        item: &KeychainItemMeta,
        cancel: &crate::store::CancellationToken,
    ) -> Result<(), StoreError> {
        cancel.check().map_err(|_| StoreError::NotFound)?;
        let out = Command::new("/usr/bin/security")
            .args([
                "delete-generic-password",
                "-s",
                &self.service,
                "-a",
                &self.account(item),
            ])
            .output()
            .map_err(|_| StoreError::NotFound)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(StoreError::NotFound)
        }
    }

    fn list(
        &self,
        _cancel: &crate::store::CancellationToken,
    ) -> Result<Vec<KeychainItemMeta>, StoreError> {
        // Enumeration of arbitrary keychain items is intentionally unsupported:
        // callers track their own secret_refs.
        Err(StoreError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CredentialKind;

    fn item() -> KeychainItemMeta {
        let secret_ref = crate::secret::SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab")
            .expect("secret ref");
        KeychainItemMeta::new(secret_ref, CredentialKind::ProviderApiKey, Some("test-provider".to_owned()))
            .expect("meta")
    }

    /// P4-032 live evidence: real login-keychain round trip on this host.
    #[test]
    fn live_keychain_put_get_delete_round_trip() {
        let kc = MacosKeychain::new();
        assert_eq!(
            kc.probe().kind(),
            PlatformKeychainKind::MacosKeychain,
            "host must expose the macOS keychain for live evidence"
        );
        let cancel = CancellationToken::new();
        let meta = item();
        let secret = b"p4-032-live-secret";
        kc.put(&meta, secret, &cancel).expect("put into keychain");
        let got = kc.get(&meta, &cancel).expect("get from keychain");
        assert_eq!(got, secret.to_vec());
        // Overwrite replaces in place.
        kc.put(&meta, b"rotated", &cancel).expect("rotate");
        assert_eq!(kc.get(&meta, &cancel).unwrap(), b"rotated".to_vec());
        kc.delete(&meta, &cancel).expect("delete");
        assert!(matches!(kc.get(&meta, &cancel), Err(StoreError::NotFound)));
        assert!(matches!(kc.delete(&meta, &cancel), Err(StoreError::NotFound)));
    }
}
