//! P4-032 production backend: macOS Keychain via the OS `security` CLI.
//! No plaintext-at-rest fallback; unavailable keychain fails closed.
#![cfg(target_os = "macos")]

use std::process::Command;

use crate::store::{
    KeychainItemMeta, KeychainProbe, PlatformKeychain, PlatformKeychainKind, StoreError,
};

/// `security -i`'s line buffer: a command line must be shorter.
const MAX_INTERACTIVE_LINE: usize = 4096;
/// `security`'s exit status for errSecItemNotFound.
const ITEM_NOT_FOUND_EXIT: i32 = 44;

/// The stored bytes from `security -g`'s `password:` value: `0x<hex>` then
/// a quoted rendering, or the printable data in quotes (unescaped), or
/// nothing for an empty item.
fn decode_password(value: &str) -> Option<Vec<u8>> {
    let value = value.trim_end_matches(['\r', '\n']);
    if let Some(hex) = value.strip_prefix("0x") {
        let hex = hex.split_whitespace().next().unwrap_or_default();
        if hex.len() % 2 != 0 {
            return None;
        }
        return (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
            .collect();
    }
    if value.is_empty() {
        return Some(Vec::new());
    }
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map(|inner| inner.as_bytes().to_vec())
}

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
        // The secret never goes on a command line (any process can read
        // another's argv): `security -i` reads the command from stdin, and
        // the secret travels hex-encoded (`-X`), so no quoting is involved.
        // `-U` updates an existing item in place: a failed write leaves the
        // previous one as it was (deleting first would lose it).
        let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
        let line = format!(
            "add-generic-password -U -s {} -a {} -X {hex}\n",
            self.service, account
        );
        // `security -i` reads a line into a 4096-byte buffer and runs what
        // does not fit as another command: a longer line would store a cut
        // key. Refused whole instead.
        if line.len() >= MAX_INTERACTIVE_LINE {
            return Err(StoreError::BoundExceeded {
                // The longest key whose line (newline included) still fits.
                limit: (MAX_INTERACTIVE_LINE - 1 - (line.len() - hex.len())) / 2,
                requested: secret.len(),
            });
        }
        let blocked = || StoreError::PersistenceBlocked {
            reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
        };
        let mut child = Command::new("/usr/bin/security")
            .arg("-i")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| blocked())?;
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().ok_or_else(blocked)?;
            stdin.write_all(line.as_bytes()).map_err(|_| blocked())?;
        }
        let out = child.wait_with_output().map_err(|_| blocked())?;
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
                "-g",
            ])
            .output()
            .map_err(|_| StoreError::NotFound)?;
        if !out.status.success() {
            // 44: errSecItemNotFound. Anything else (a locked keychain, no
            // access) is not "nothing stored".
            return Err(if out.status.code() == Some(ITEM_NOT_FOUND_EXIT) {
                StoreError::NotFound
            } else {
                StoreError::PersistenceBlocked {
                    reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
                }
            });
        }
        // `-g` prints `password: "<data>"` for printable data and
        // `password: 0x<hex>  "<escaped>"` otherwise (`-w` would print the
        // bare hex, indistinguishable from a key made of hex digits): the
        // bytes as stored, either way.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let value = stderr
            .lines()
            .find_map(|line| line.strip_prefix("password:"))
            .map(str::trim_start)
            .ok_or(StoreError::InvalidMetadata)?;
        decode_password(value).ok_or(StoreError::InvalidMetadata)
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
    use crate::store::{CancellationToken, CredentialKind};

    fn item() -> KeychainItemMeta {
        let secret_ref = crate::secret::SecretRef::from_id("018f3c8a-7e2b-7a10-8c4d-0123456789ab")
            .expect("secret ref");
        KeychainItemMeta::new(
            secret_ref,
            CredentialKind::ProviderApiKey,
            Some("test-provider".to_owned()),
        )
        .expect("meta")
    }

    #[test]
    fn a_password_line_decodes_to_the_bytes_stored() {
        assert_eq!(
            decode_password(r#""plain key""#),
            Some(b"plain key".to_vec())
        );
        assert_eq!(
            decode_password(r#""it's a "key" $HOME""#),
            Some(br#"it's a "key" $HOME"#.to_vec())
        );
        assert_eq!(
            decode_password(r#"0x01FF78  "\001\377x""#),
            Some(vec![0x01, 0xff, b'x'])
        );
        assert_eq!(decode_password(""), Some(Vec::new()));
        assert_eq!(decode_password("0x0"), None);
        assert_eq!(decode_password("unquoted"), None);
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
        // A key too long for one `security -i` line is refused whole, and the
        // stored one is untouched.
        assert!(matches!(
            kc.put(&meta, &[b'k'; 3000], &cancel),
            Err(StoreError::BoundExceeded { .. })
        ));
        assert_eq!(kc.get(&meta, &cancel).unwrap(), b"rotated".to_vec());
        // Bytes that are not printable come back as they were stored (not as
        // the hex `-w` would print).
        let raw = [0x01_u8, 0xff, b'x', b'"'];
        kc.put(&meta, &raw, &cancel).expect("raw");
        assert_eq!(kc.get(&meta, &cancel).unwrap(), raw.to_vec());
        // A key made of hex digits is text, not hex.
        kc.put(&meta, b"0a1b2c", &cancel).expect("hex digits");
        assert_eq!(kc.get(&meta, &cancel).unwrap(), b"0a1b2c".to_vec());
        // Quotes, spaces and shell characters are data, not syntax.
        let odd = b"it's a \"key\" $HOME -w x";
        kc.put(&meta, odd, &cancel).expect("odd");
        assert_eq!(kc.get(&meta, &cancel).unwrap(), odd.to_vec());
        kc.delete(&meta, &cancel).expect("delete");
        assert!(matches!(kc.get(&meta, &cancel), Err(StoreError::NotFound)));
        assert!(matches!(
            kc.delete(&meta, &cancel),
            Err(StoreError::NotFound)
        ));
    }
}
