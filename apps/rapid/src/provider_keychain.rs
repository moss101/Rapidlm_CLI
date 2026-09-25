//! Provider API keys in the OS keychain (SEAM-02-3): `rapid setup
//! --key-stdin` stores a key under an alias, the config names the alias
//! (`[model.<id>] keychain = "<alias>"`), and a run reads it when it builds
//! the model client — the executor boundary (invariant 11: a handle until
//! then). The backend is the platform's own (`auth::os_keychain_select`);
//! where there is none the failure is typed, never a plaintext fallback.

use std::sync::Arc;

use auth::{CancellationToken, CredentialKind, KeychainItemMeta, PlatformKeychain, SecretRef};

/// Why a keychain operation did not happen. Never carries a key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeychainError {
    /// This platform has no keychain rapid can use, or it refused access.
    Unavailable,
    /// Nothing is stored under the alias.
    NotFound { alias: String },
    /// The alias is not one a keychain item can be named by.
    InvalidAlias { alias: String },
    /// The stored value is not a key a request can carry (not UTF-8).
    Unreadable { alias: String },
    /// A key with characters no HTTP header carries (or empty): not stored,
    /// since what the keychain gives back for it is not the key.
    UnsendableKey,
    /// Longer than the keychain takes in one piece: not stored.
    TooLong,
}

impl std::fmt::Display for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => {
                write!(f, "the OS keychain is not available on this system")
            }
            Self::NotFound { alias } => {
                write!(f, "the OS keychain holds no key under '{alias}'")
            }
            Self::InvalidAlias { alias } => {
                write!(f, "'{alias}' is not a keychain alias")
            }
            Self::Unreadable { alias } => {
                write!(f, "the key under '{alias}' in the OS keychain is not text")
            }
            Self::UnsendableKey => write!(
                f,
                "the key has characters no HTTP header can carry (a line break from a file?)"
            ),
            Self::TooLong => write!(f, "the key is longer than the OS keychain takes"),
        }
    }
}

impl std::error::Error for KeychainError {}

#[cfg(test)]
thread_local! {
    /// A test's keychain: tests never touch the real one.
    static TEST_BACKEND: std::cell::RefCell<Option<Arc<dyn PlatformKeychain>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `body` with `backend` as this thread's keychain (tests only).
#[cfg(test)]
pub fn with_backend<R>(backend: Arc<dyn PlatformKeychain>, body: impl FnOnce() -> R) -> R {
    let previous = TEST_BACKEND.with(|slot| slot.borrow_mut().replace(backend));
    let result = body();
    TEST_BACKEND.with(|slot| *slot.borrow_mut() = previous);
    result
}

fn backend() -> Result<Arc<dyn PlatformKeychain>, KeychainError> {
    #[cfg(test)]
    if let Some(backend) = TEST_BACKEND.with(|slot| slot.borrow().clone()) {
        return Ok(backend);
    }
    #[cfg(test)]
    {
        // A test that did not install a keychain must not reach the real one.
        Err(KeychainError::Unavailable)
    }
    #[cfg(not(test))]
    {
        auth::os_keychain_select::production_backend(std::env::consts::OS)
            .map(Arc::from)
            .map_err(|_| KeychainError::Unavailable)
    }
}

fn item(alias: &str) -> Result<KeychainItemMeta, KeychainError> {
    let invalid = || KeychainError::InvalidAlias {
        alias: alias.to_owned(),
    };
    let refer = SecretRef::from_alias(alias).map_err(|_| invalid())?;
    KeychainItemMeta::new(refer, CredentialKind::ProviderApiKey, None).map_err(|_| invalid())
}

/// The key stored under `alias`.
pub fn read(alias: &str) -> Result<String, KeychainError> {
    let backend = backend()?;
    if !backend.probe().is_available() {
        return Err(KeychainError::Unavailable);
    }
    let bytes = backend
        .get(&item(alias)?, &CancellationToken::new())
        .map_err(|err| match err {
            auth::StoreError::NotFound => KeychainError::NotFound {
                alias: alias.to_owned(),
            },
            _ => KeychainError::Unavailable,
        })?;
    String::from_utf8(bytes).map_err(|_| KeychainError::Unreadable {
        alias: alias.to_owned(),
    })
}

/// What was under an alias before [`store`] replaced it, so a failed write
/// after it can put it back — byte for byte, whatever it was.
#[derive(Debug)]
pub enum Previous {
    Nothing,
    Bytes(Vec<u8>),
}

/// Whether [`store`] changed anything.
#[derive(Debug)]
pub enum Stored {
    /// The same key was already there: nothing was written.
    Unchanged,
    /// Written; this is what was there before.
    Replaced(Previous),
}

/// Store `key` under `alias`, unless it is already there. A key a request
/// could not carry is refused: printable ASCII only (what an HTTP header
/// takes, and what every keychain gives back as it was put). A failed write
/// puts back what was there.
pub fn store(alias: &str, key: &str) -> Result<Stored, KeychainError> {
    if key.is_empty() || !key.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(KeychainError::UnsendableKey);
    }
    let backend = backend()?;
    if !backend.probe().is_available() {
        return Err(KeychainError::Unavailable);
    }
    let item = item(alias)?;
    let cancel = CancellationToken::new();
    let previous = match backend.get(&item, &cancel) {
        Ok(bytes) if bytes == key.as_bytes() => return Ok(Stored::Unchanged),
        Ok(bytes) => Previous::Bytes(bytes),
        Err(auth::StoreError::NotFound) => Previous::Nothing,
        Err(_) => return Err(KeychainError::Unavailable),
    };
    if let Err(err) = backend.put(&item, key.as_bytes(), &cancel) {
        let _ = restore(alias, previous);
        return Err(match err {
            auth::StoreError::BoundExceeded { .. } => KeychainError::TooLong,
            _ => KeychainError::Unavailable,
        });
    }
    Ok(Stored::Replaced(previous))
}

/// Put back what [`store`] replaced (best effort: the caller is already
/// reporting a failure).
pub fn restore(alias: &str, previous: Previous) -> Result<(), KeychainError> {
    let backend = backend()?;
    let item = item(alias)?;
    let cancel = CancellationToken::new();
    match previous {
        Previous::Bytes(old) => backend
            .put(&item, &old, &cancel)
            .map_err(|_| KeychainError::Unavailable),
        Previous::Nothing => match backend.delete(&item, &cancel) {
            Ok(()) | Err(auth::StoreError::NotFound) => Ok(()),
            Err(_) => Err(KeychainError::Unavailable),
        },
    }
}

/// An in-memory keychain for tests.
#[cfg(test)]
pub mod testing {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use auth::{
        CancellationToken, KeychainItemMeta, KeychainProbe, PlatformKeychain, PlatformKeychainKind,
        StoreError,
    };

    #[derive(Default)]
    pub struct MemoryKeychain {
        pub items: Mutex<BTreeMap<String, Vec<u8>>>,
        pub puts: Mutex<usize>,
        pub unavailable: bool,
        /// The next this many puts fail after dropping what was stored (as
        /// a backend that deletes before adding would).
        pub failing_puts: Mutex<usize>,
    }

    impl MemoryKeychain {
        pub fn unavailable() -> Self {
            Self {
                unavailable: true,
                ..Self::default()
            }
        }

        pub fn get_text(&self, account: &str) -> Option<String> {
            self.items
                .lock()
                .expect("lock")
                .get(account)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        }
    }

    impl PlatformKeychain for MemoryKeychain {
        fn probe(&self) -> KeychainProbe {
            if self.unavailable {
                KeychainProbe::unavailable(PlatformKeychainKind::Unsupported)
            } else {
                KeychainProbe::available(PlatformKeychainKind::Unsupported)
            }
        }

        fn put(
            &self,
            item: &KeychainItemMeta,
            secret: &[u8],
            _cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            *self.puts.lock().expect("lock") += 1;
            let mut failing = self.failing_puts.lock().expect("lock");
            if *failing > 0 {
                *failing -= 1;
                self.items.lock().expect("lock").remove(item.account());
                return Err(StoreError::PersistenceBlocked {
                    reason: auth::PersistenceBlockReason::KeychainUnavailable,
                });
            }
            self.items
                .lock()
                .expect("lock")
                .insert(item.account().to_owned(), secret.to_vec());
            Ok(())
        }

        fn get(
            &self,
            item: &KeychainItemMeta,
            _cancel: &CancellationToken,
        ) -> Result<Vec<u8>, StoreError> {
            self.items
                .lock()
                .expect("lock")
                .get(item.account())
                .cloned()
                .ok_or(StoreError::NotFound)
        }

        fn delete(
            &self,
            item: &KeychainItemMeta,
            _cancel: &CancellationToken,
        ) -> Result<(), StoreError> {
            self.items
                .lock()
                .expect("lock")
                .remove(item.account())
                .map(|_| ())
                .ok_or(StoreError::NotFound)
        }

        fn list(&self, _cancel: &CancellationToken) -> Result<Vec<KeychainItemMeta>, StoreError> {
            Err(StoreError::NotFound)
        }
    }
}
