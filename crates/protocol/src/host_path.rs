//! Host filesystem path resolution that is stable across platforms.
//!
//! `std::fs::canonicalize` on Windows returns a *verbatim* path
//! (`\\?\C:\Users\…`). That form is a different `Path` prefix component
//! from the plain `C:\Users\…` every other API in this workspace produces
//! and compares against (`Path::starts_with` confinement checks, string
//! identities, `git` argv, `core.hooksPath`, …), so a canonicalized path
//! and a plain one that name the very same directory disagree. Every
//! caller that canonicalizes for identity or confinement goes through
//! [`canonicalize`] here, which returns the plain form — identical to
//! `std::fs::canonicalize` on Unix.

use std::io;
use std::path::{Path, PathBuf};

/// `std::fs::canonicalize` in the plain (non-verbatim) form on every platform.
///
/// The path still resolves symlinks and `..` through the OS exactly as
/// `std::fs::canonicalize` does; only the Windows `\\?\` verbatim prefix is
/// removed when the result is an ordinary drive path. A verbatim UNC path
/// (`\\?\UNC\server\share`) is returned as `\\server\share`; anything else
/// is returned unchanged.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map(|resolved| simplified(&resolved))
}

/// Strip the Windows verbatim prefix from an already-resolved path.
///
/// `\\?\C:\dir` → `C:\dir`; `\\?\UNC\srv\share\dir` → `\\srv\share\dir`.
/// Paths without a verbatim prefix — every path on Unix — are returned as
/// they are. The plain form is what every downstream comparison and every
/// child process (`git`, shells) expects; nothing in this workspace relies
/// on the verbatim form's ability to exceed `MAX_PATH`.
pub fn simplified(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    match simplified_str(text) {
        Some(plain) => PathBuf::from(plain),
        None => path.to_path_buf(),
    }
}

/// String form of [`simplified`]: `Some(plain)` when a verbatim prefix was
/// removed, `None` when the input was not a verbatim path.
pub fn simplified_str(text: &str) -> Option<String> {
    let rest = text.strip_prefix(r"\\?\")?;
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    if is_drive_path(rest) {
        return Some(rest.to_owned());
    }
    None
}

/// `true` for a Windows verbatim disk path (`\\?\C:\…`).
pub fn is_verbatim_drive(text: &str) -> bool {
    text.strip_prefix(r"\\?\").is_some_and(is_drive_path)
}

fn is_drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || matches!(bytes[2], b'\\' | b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_drive_prefix_is_removed() {
        assert_eq!(
            simplified_str(r"\\?\C:\Users\runner\proj").as_deref(),
            Some(r"C:\Users\runner\proj")
        );
        assert_eq!(simplified_str(r"\\?\D:").as_deref(), Some("D:"));
        assert!(is_verbatim_drive(r"\\?\C:\x"));
        assert!(is_verbatim_drive(r"\\?\c:"));
    }

    #[test]
    fn verbatim_unc_becomes_plain_unc() {
        assert_eq!(
            simplified_str(r"\\?\UNC\server\share\dir").as_deref(),
            Some(r"\\server\share\dir")
        );
    }

    #[test]
    fn non_verbatim_paths_are_untouched() {
        for sample in [
            "/tmp/x",
            r"C:\plain",
            r"\\server\share",
            r"\\.\pipe\rapidlm",
            r"\\?\pipe\x",
            r"\\?\CD\x",
            "",
        ] {
            assert_eq!(simplified_str(sample), None, "{sample:?}");
            assert_eq!(simplified(Path::new(sample)), PathBuf::from(sample));
            assert!(!is_verbatim_drive(sample), "{sample:?}");
        }
    }

    #[test]
    fn canonicalize_matches_std_on_unix_and_is_plain_on_windows() {
        let dir = std::env::temp_dir();
        let ours = canonicalize(&dir).expect("temp dir canonicalizes");
        let theirs = std::fs::canonicalize(&dir).expect("std canonicalizes");
        assert_eq!(ours, simplified(&theirs));
        if cfg!(unix) {
            // Identical to std on Unix: there is no prefix to strip there.
            assert_eq!(ours, theirs);
        } else {
            assert!(theirs.to_string_lossy().starts_with(r"\\?\"));
        }
        assert!(!ours.to_string_lossy().starts_with(r"\\?\"));
        assert!(ours.is_absolute());
        assert!(ours.is_dir());
    }
}
