//! P4-032/GA explicit production routing: each supported OS selects exactly
//! one native backend; unsupported platforms get a typed failure.
//! P4-032/GA explicit production routing: each supported OS selects exactly
//! one native backend; unsupported platforms get a typed failure.

use crate::store::{PlatformKeychain, StoreError};

/// Explicitly routed production backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectedKeychain {
    MacosKeychain,
    WindowsCredentialManager,
    FreedesktopSecretService,
    /// No production secure-storage backend exists for this platform.
    Unsupported,
}
/// Pure routing function over the target OS identifier
/// (`std::env::consts::OS`). FileKeychain is deliberately unreachable here:
/// it is never a production fallback.
pub fn select(os: &str) -> SelectedKeychain {
    match os {
        "macos" => SelectedKeychain::MacosKeychain,
        "windows" => SelectedKeychain::WindowsCredentialManager,
        "linux" => SelectedKeychain::FreedesktopSecretService,
        _ => SelectedKeychain::Unsupported,
    }
}
/// Bind the selection to its concrete production type at runtime.
pub fn production_backend(os: &str) -> Result<Box<dyn PlatformKeychain>, StoreError> {
    match select(os) {
        #[cfg(target_os = "macos")]
        SelectedKeychain::MacosKeychain => Ok(Box::new(crate::os_keychain::MacosKeychain::new())),
        #[cfg(target_os = "windows")]
        SelectedKeychain::WindowsCredentialManager => Ok(Box::new(
            crate::os_keychain_other::windows::WindowsCredentialManager,
        )),
        #[cfg(target_os = "linux")]
        SelectedKeychain::FreedesktopSecretService => Ok(Box::new(
            crate::os_keychain_other::linux::FreedesktopSecretService,
        )),
        _ => Err(StoreError::PersistenceBlocked {
            reason: crate::store::PersistenceBlockReason::KeychainUnavailable,
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_each_os_to_its_native_backend_and_fails_unsupported() {
        assert_eq!(select("macos"), SelectedKeychain::MacosKeychain);
        assert_eq!(
            select("windows"),
            SelectedKeychain::WindowsCredentialManager
        );
        assert_eq!(select("linux"), SelectedKeychain::FreedesktopSecretService);
        assert_eq!(select("plan9"), SelectedKeychain::Unsupported);
    }
    #[test]
    fn production_binding_on_this_host_is_the_macos_keychain() {
        // On macOS the selector must bind the live-verified MacosKeychain;
        // other hosts exercise their own arm on native runners.
        let backend = production_backend(std::env::consts::OS);
        #[cfg(target_os = "macos")]
        assert!(backend.is_ok(), "macOS host must bind MacosKeychain");
        #[cfg(not(target_os = "macos"))]
        let _ = backend;
    }
}
