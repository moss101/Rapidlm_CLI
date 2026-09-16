//! The environment a child process needs to start at all on this host.
//!
//! Tool-spawned children run with a cleared environment plus a short
//! allowlist, so nothing ambient (credentials, tokens) reaches them. On
//! Windows a completely bare environment is not merely spartan but broken:
//! the loader and `cmd.exe` locate the system through `SystemRoot`,
//! `COMSPEC` and `PATHEXT`, `taskkill` cannot initialise without
//! `SystemRoot`, and the MSYS runtime behind Git for Windows' tools wants
//! `TEMP`/`TMP`. None of these carries a secret. This is the one list every
//! `env_clear()` site adds back, so the platform contract is stated once.

/// Variables forwarded from the ambient environment on Windows, when set.
pub const WINDOWS_BASE_ENV: &[&str] = &[
    "SystemRoot",
    "SYSTEMDRIVE",
    "windir",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
];

/// The platform-required variables present in this process's environment:
/// [`WINDOWS_BASE_ENV`] on Windows, nothing elsewhere.
pub fn platform_base_env() -> Vec<(String, String)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    WINDOWS_BASE_ENV
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_adds_nothing_and_windows_forwards_only_the_listed_names() {
        let pairs = platform_base_env();
        if cfg!(windows) {
            assert!(
                pairs.iter().any(|(key, _)| key == "SystemRoot"),
                "every Windows host has SystemRoot: {pairs:?}"
            );
            for (key, value) in &pairs {
                assert!(WINDOWS_BASE_ENV.contains(&key.as_str()), "{key}");
                assert!(!value.is_empty());
            }
        } else {
            assert!(pairs.is_empty());
        }
        // Never a credential-shaped name, on any host.
        for key in WINDOWS_BASE_ENV {
            let upper = key.to_ascii_uppercase();
            assert!(
                !upper.contains("KEY") && !upper.contains("TOKEN") && !upper.contains("SECRET")
            );
        }
    }
}
