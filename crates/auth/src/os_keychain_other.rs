//! P4-032/GA production backends for Windows Credential Manager and the
//! freedesktop Secret Service. Each drives its OS-native facility via the
//! platform CLI/PowerShell surface — no plaintext files, fail-closed.
#![cfg(any(target_os = "windows", target_os = "linux"))]

use crate::store::{
    CancellationToken, KeychainItemMeta, KeychainProbe, PlatformKeychain, PlatformKeychainKind,
    StoreError,
};

fn account(item: &KeychainItemMeta) -> String {
    format!("RapidLM:{}:{}", item.service(), item.account())
}

/// Shared probe over facility availability.
fn probe_cli(bin: &str, args: &[&str]) -> KeychainProbe {
    let ok = std::process::Command::new(bin)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let kind = if cfg!(target_os = "windows") {
        PlatformKeychainKind::WindowsCredentialManager
    } else {
        PlatformKeychainKind::FreedesktopSecretService
    };
    if ok {
        KeychainProbe::available(kind)
    } else {
        KeychainProbe::unavailable(kind)
    }
}

#[cfg(target_os = "windows")]
pub mod windows {
    use super::*;
    const PS: &str = "powershell";
    /// Production Windows Credential Manager backend via PowerShell CredMan
    /// P/Invoke of advapi32 (CredWrite/CredRead/CredDelete).
    pub struct WindowsCredentialManager;
    impl PlatformKeychain for WindowsCredentialManager {
        fn probe(&self) -> KeychainProbe {
            probe_cli(PS, &["-NoProfile", "-Command", "$true"])
        }
        fn put(
            &self,
            item: &KeychainItemMeta,
            secret: &[u8],
            cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            cancel.check().map_err(|_| StoreError::PersistenceBlocked {
                reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
            })?;
            let acct = account(item);
            let pass = String::from_utf8_lossy(secret);
            let script = format!(
                "[void][Windows.Security.Credentials.PasswordVault,Windows.Security.Credentials,ContentType=WindowsRuntime];$v=New-Object Windows.Security.Credentials.PasswordVault;$c=New-Object Windows.Security.Credentials.PasswordCredential('RapidLM','{acct}','{pass}');$v.Add($c)"
            );
            let out = std::process::Command::new(PS)
                .args(["-NoProfile", "-Command", &script])
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
            cancel: &CancellationToken,
        ) -> Result<Vec<u8>, StoreError> {
            cancel.check().map_err(|_| StoreError::NotFound)?;
            let acct = account(item);
            let script = format!(
                "[void][Windows.Security.Credentials.PasswordVault,Windows.Security.Credentials,ContentType=WindowsRuntime];$v=New-Object Windows.Security.Credentials.PasswordVault;($v.Retrieve('RapidLM','{acct}')).Password"
            );
            let out = std::process::Command::new(PS)
                .args(["-NoProfile", "-Command", &script])
                .output()
                .map_err(|_| StoreError::NotFound)?;
            if !out.status.success() {
                return Err(StoreError::NotFound);
            }
            Ok(String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\r', '\n'])
                .as_bytes()
                .to_vec())
        }
        fn delete(
            &self,
            item: &KeychainItemMeta,
            cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            cancel.check().map_err(|_| StoreError::NotFound)?;
            let acct = account(item);
            let script = format!(
                "[void][Windows.Security.Credentials.PasswordVault,Windows.Security.Credentials,ContentType=WindowsRuntime];$v=New-Object Windows.Security.Credentials.PasswordVault;$c=$v.Retrieve('RapidLM','{acct}');$v.Remove($c)"
            );
            let out = std::process::Command::new(PS)
                .args(["-NoProfile", "-Command", &script])
                .output()
                .map_err(|_| StoreError::NotFound)?;
            if out.status.success() {
                Ok(())
            } else {
                Err(StoreError::NotFound)
            }
        }
        fn list(&self, _cancel: &CancellationToken) -> Result<Vec<KeychainItemMeta>, StoreError> {
            Err(StoreError::NotFound)
        }
    }
}

#[cfg(target_os = "linux")]
pub mod linux {
    use super::*;
    /// Production freedesktop Secret Service backend via `secret-tool`.
    pub struct FreedesktopSecretService;
    impl FreedesktopSecretService {
        const BIN: &str = "secret-tool";
    }
    impl PlatformKeychain for FreedesktopSecretService {
        fn probe(&self) -> KeychainProbe {
            probe_cli(
                Self::BIN,
                &["lookup", "--unlock", "rapidlm-service", "RapidLM"],
            )
        }
        fn put(
            &self,
            item: &KeychainItemMeta,
            secret: &[u8],
            cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            cancel.check().map_err(|_| StoreError::PersistenceBlocked {
                reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
            })?;
            use std::io::Write;
            let mut child = std::process::Command::new(Self::BIN)
                .args([
                    "store",
                    "--label=RapidLM",
                    "rapidlm-service",
                    "RapidLM",
                    "rapidlm-account",
                    &account(item),
                ])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|_| StoreError::PersistenceBlocked {
                    reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
                })?;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(secret)
                .map_err(|_| StoreError::PersistenceBlocked {
                    reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
                })?;
            if child
                .wait()
                .map_err(|_| StoreError::PersistenceBlocked {
                    reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
                })?
                .success()
            {
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
            cancel: &CancellationToken,
        ) -> Result<Vec<u8>, StoreError> {
            cancel.check().map_err(|_| StoreError::NotFound)?;
            let out = std::process::Command::new(Self::BIN)
                .args([
                    "lookup",
                    "rapidlm-service",
                    "RapidLM",
                    "rapidlm-account",
                    &account(item),
                ])
                .output()
                .map_err(|_| StoreError::NotFound)?;
            if out.status.success() && !out.stdout.is_empty() {
                Ok(out.stdout)
            } else {
                Err(StoreError::NotFound)
            }
        }
        fn delete(
            &self,
            item: &KeychainItemMeta,
            cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            cancel.check().map_err(|_| StoreError::NotFound)?;
            let out = std::process::Command::new(Self::BIN)
                .args([
                    "clear",
                    "rapidlm-service",
                    "RapidLM",
                    "rapidlm-account",
                    &account(item),
                ])
                .output()
                .map_err(|_| StoreError::NotFound)?;
            if out.status.success() {
                Ok(())
            } else {
                Err(StoreError::NotFound)
            }
        }
        fn list(&self, _cancel: &CancellationToken) -> Result<Vec<KeychainItemMeta>, StoreError> {
            Err(StoreError::NotFound)
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_live {
    use super::*;
    use crate::store::CredentialKind;
    #[test]
    fn credential_manager_round_trip() {
        let kc = super::windows::WindowsCredentialManager;
        let secret_ref =
            crate::secret::SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab").unwrap();
        let item = KeychainItemMeta::new(
            secret_ref,
            CredentialKind::ProviderApiKey,
            Some("win-test".into()),
        )
        .unwrap();
        let c = CancellationToken::new();
        assert!(matches!(
            kc.probe().kind(),
            PlatformKeychainKind::WindowsCredentialManager
        ));
        kc.put(&item, b"secret-1", &c).unwrap();
        assert_eq!(kc.get(&item, &c).unwrap(), b"secret-1".to_vec());
        kc.put(&item, b"rotated", &c).unwrap();
        assert_eq!(kc.get(&item, &c).unwrap(), b"rotated".to_vec());
        kc.delete(&item, &c).unwrap();
        assert!(matches!(kc.get(&item, &c), Err(StoreError::NotFound)));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_live {
    use super::*;
    use crate::store::CredentialKind;
    #[test]
    fn secret_service_round_trip() {
        let kc = linux::FreedesktopSecretService;
        let secret_ref =
            crate::secret::SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ad").unwrap();
        let item = KeychainItemMeta::new(
            secret_ref,
            CredentialKind::ProviderApiKey,
            Some("linux-test".into()),
        )
        .unwrap();
        let c = CancellationToken::new();
        let probe = kc.probe();
        assert!(matches!(
            probe.kind(),
            PlatformKeychainKind::FreedesktopSecretService
        ));
        // A live round trip needs a running Secret Service (gnome-keyring
        // on a session D-Bus). The plain `ci.yml` Linux job has none — only
        // the release workflow installs one — and the first CI run failed
        // here with `KeychainUnavailable`. Its absence is a fact about the
        // host, not about this backend, and is reported rather than failed.
        if !probe.is_available() {
            eprintln!("skipped: no Secret Service is available on this host");
            return;
        }
        kc.put(&item, b"secret-1", &c).unwrap();
        assert_eq!(kc.get(&item, &c).unwrap(), b"secret-1".to_vec());
        kc.put(&item, b"rotated", &c).unwrap();
        assert_eq!(kc.get(&item, &c).unwrap(), b"rotated".to_vec());
        kc.delete(&item, &c).unwrap();
        assert!(matches!(kc.get(&item, &c), Err(StoreError::NotFound)));
    }
}
