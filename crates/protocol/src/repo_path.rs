//! Normalized repository-relative path.
//!
//! Construction is platform-independent: `/` and `\` are both separators,
//! Windows drive/UNC forms are rejected on every host, and `..` is never
//! rewritten into an in-repo path.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Maximum UTF-8 bytes accepted in a raw or normalized repository path.
pub const MAX_REPO_PATH_BYTES: usize = 4096;

/// Repository-relative logical path. Wire form is a `/`-separated string.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RepoPath(String);

/// Parse failure for a non-repository-relative path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoPathError {
    Empty,
    TooLong,
    Nul,
    Control,
    Absolute,
    WindowsDrive,
    Unc,
    Traversal,
}

impl RepoPath {
    /// Parse a repository-relative path and normalize separators to `/`.
    pub fn parse(input: &str) -> Result<Self, RepoPathError> {
        if input.is_empty() {
            return Err(RepoPathError::Empty);
        }
        if input.len() > MAX_REPO_PATH_BYTES {
            return Err(RepoPathError::TooLong);
        }
        if input.contains('\0') {
            return Err(RepoPathError::Nul);
        }
        if input.chars().any(char::is_control) {
            return Err(RepoPathError::Control);
        }
        if is_windows_drive(input) {
            return Err(RepoPathError::WindowsDrive);
        }
        if is_unc(input) {
            return Err(RepoPathError::Unc);
        }
        if is_absolute_root(input) {
            return Err(RepoPathError::Absolute);
        }

        let mut parts = Vec::new();
        for component in input.split(['/', '\\']) {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                return Err(RepoPathError::Traversal);
            }
            parts.push(component);
        }
        if parts.is_empty() {
            return Err(RepoPathError::Empty);
        }

        let normalized = parts.join("/");
        if normalized.len() > MAX_REPO_PATH_BYTES {
            return Err(RepoPathError::TooLong);
        }
        Ok(Self(normalized))
    }

    /// Canonical `/`-separated logical path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Path components after normalization.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RepoPath {
    type Err = RepoPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for RepoPath {
    type Error = RepoPathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for RepoPath {
    type Error = RepoPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl AsRef<str> for RepoPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for RepoPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(RepoPathVisitor)
    }
}

struct RepoPathVisitor;

impl Visitor<'_> for RepoPathVisitor {
    type Value = RepoPath;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a repository-relative path using / separators")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        RepoPath::parse(value).map_err(E::custom)
    }
}

impl fmt::Display for RepoPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty repository path",
            Self::TooLong => "repository path exceeds the maximum length",
            Self::Nul => "NUL byte in repository path",
            Self::Control => "control character in repository path",
            Self::Absolute => "absolute repository path",
            Self::WindowsDrive => "Windows drive path",
            Self::Unc => "UNC path",
            Self::Traversal => "path traversal",
        })
    }
}

impl Error for RepoPathError {}

fn is_windows_drive(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_unc(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 2 && matches!(bytes[0], b'/' | b'\\') && matches!(bytes[1], b'/' | b'\\')
}

fn is_absolute_root(input: &str) -> bool {
    input.starts_with('/') || input.starts_with('\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GOLDEN_JSON: &str = r#""src/lib.rs""#;

    fn assert_rejected(input: &str, err: RepoPathError) {
        assert_eq!(RepoPath::parse(input), Err(err), "parse accepted {input:?}");
        assert_eq!(
            input.parse::<RepoPath>(),
            Err(err),
            "FromStr accepted {input:?}"
        );
        assert!(
            serde_json::from_str::<RepoPath>(&serde_json::to_string(input).expect("quote"))
                .is_err(),
            "serde accepted {input:?}"
        );
    }

    #[test]
    fn parse_accepts_relative_file() {
        let path = RepoPath::parse("src/lib.rs").expect("relative path");
        assert_eq!(path.as_str(), "src/lib.rs");
        assert_eq!(path.to_string(), "src/lib.rs");
        assert_eq!(path.components().collect::<Vec<_>>(), ["src", "lib.rs"]);
    }

    #[test]
    fn serialization_uses_logical_slash_separators() {
        let from_slash = RepoPath::parse("src/main.rs").expect("slash");
        let from_backslash = RepoPath::parse("src\\main.rs").expect("backslash");
        let from_mixed = RepoPath::parse("crates\\protocol/src/lib.rs").expect("mixed");
        assert_eq!(from_slash.as_str(), "src/main.rs");
        assert_eq!(from_backslash.as_str(), "src/main.rs");
        assert_eq!(from_mixed.as_str(), "crates/protocol/src/lib.rs");
        assert_eq!(
            serde_json::to_string(&from_backslash).expect("serialize"),
            r#""src/main.rs""#
        );
        assert!(!from_backslash.as_str().contains('\\'));
        assert!(!from_mixed.as_str().contains('\\'));
    }

    #[test]
    fn golden_json_round_trips() {
        let path = RepoPath::parse("src/lib.rs").expect("parse");
        let json = serde_json::to_string(&path).expect("serialize");
        assert_eq!(json, GOLDEN_JSON);
        let decoded = serde_json::from_str::<RepoPath>(GOLDEN_JSON).expect("deserialize");
        assert_eq!(decoded, path);
        assert_eq!(decoded.as_str(), "src/lib.rs");
    }

    #[test]
    fn current_dir_components_are_normalized() {
        let path = RepoPath::parse("./src/./lib.rs").expect("dot components");
        assert_eq!(path.as_str(), "src/lib.rs");
        assert_eq!(RepoPath::parse("src/lib.rs/").expect("trailing"), path);
        assert_eq!(
            RepoPath::parse("src//lib.rs").expect("empty component"),
            path
        );
    }

    #[test]
    fn rejects_traversal() {
        for sample in [
            "../x",
            "..",
            "foo/../bar",
            "src/../../etc/passwd",
            "src\\..\\secret",
            "src/lib.rs/../../../etc/passwd",
            "foo/bar/..",
            "./../x",
        ] {
            assert_rejected(sample, RepoPathError::Traversal);
        }
    }

    #[test]
    fn rejects_unix_absolute_paths() {
        for sample in [
            "/etc/passwd",
            "/src/lib.rs",
            "/",
            "/../etc/passwd",
            "\\windows\\system32",
            "\\",
        ] {
            assert_rejected(sample, RepoPathError::Absolute);
        }
    }

    #[test]
    fn rejects_windows_drive_syntax() {
        for sample in [
            r"C:\Windows\System32",
            "C:/Windows/System32",
            r"c:\foo",
            "D:foo",
            "Z:",
            r"C:\",
            "C:/",
            r"c:src\lib.rs",
        ] {
            assert_rejected(sample, RepoPathError::WindowsDrive);
        }
    }

    #[test]
    fn rejects_unc_syntax() {
        for sample in [
            r"\\server\share",
            "//server/share",
            r"\\?\C:\Windows",
            r"\\.\pipe\rapidlm",
            r"/\server\share",
            r"\/server/share",
            "//",
            r"\\",
        ] {
            assert_rejected(sample, RepoPathError::Unc);
        }
    }

    #[test]
    fn rejects_empty_and_dot_only() {
        assert_rejected("", RepoPathError::Empty);
        assert_rejected(".", RepoPathError::Empty);
        assert_rejected("./.", RepoPathError::Empty);
    }

    #[test]
    fn rejects_nul_and_control_characters() {
        assert_rejected("src/\0lib.rs", RepoPathError::Nul);
        assert_rejected("src/\nlib.rs", RepoPathError::Control);
        assert_rejected("src/\tlib.rs", RepoPathError::Control);
        assert_rejected("src/\u{7f}lib.rs", RepoPathError::Control);
    }

    #[test]
    fn rejects_oversized_input() {
        let oversized = "a".repeat(MAX_REPO_PATH_BYTES + 1);
        assert_eq!(RepoPath::parse(&oversized), Err(RepoPathError::TooLong));
    }

    #[test]
    fn error_display_does_not_echo_input() {
        let err = RepoPath::parse("../secret").expect_err("traversal");
        let shown = err.to_string();
        assert_eq!(shown, "path traversal");
        assert!(!shown.contains("secret"));
        assert!(!shown.contains(".."));
    }

    #[test]
    fn display_and_from_str_round_trip() {
        let path: RepoPath = "docs/02-SDD.md".parse().expect("parse");
        assert_eq!(
            path.to_string().parse::<RepoPath>().expect("round-trip"),
            path
        );
    }

    proptest! {
        #[test]
        fn safe_relative_components_round_trip(
            parts in prop::collection::vec("[A-Za-z0-9][A-Za-z0-9._-]{0,16}", 1..8)
        ) {
            let input = parts.join("/");
            let path = RepoPath::parse(&input).expect("safe relative path");
            prop_assert_eq!(path.as_str(), input.as_str());
            prop_assert!(!path.as_str().contains('\\'));
            prop_assert!(!path.as_str().starts_with('/'));
            prop_assert!(path.components().all(|c| c != ".." && c != "." && !c.is_empty()));
            let json = serde_json::to_string(&path).expect("serialize");
            prop_assert!(!json.contains('\\'));
            let decoded = serde_json::from_str::<RepoPath>(&json).expect("deserialize");
            prop_assert_eq!(decoded, path);
        }

        #[test]
        fn inserted_dotdot_component_is_rejected(
            prefix in prop::collection::vec("[A-Za-z0-9]{1,8}", 0..4),
            suffix in prop::collection::vec("[A-Za-z0-9]{1,8}", 0..4)
        ) {
            let mut parts = prefix;
            parts.push("..".to_owned());
            parts.extend(suffix);
            let input = parts.join("/");
            prop_assert_eq!(RepoPath::parse(&input), Err(RepoPathError::Traversal));
        }
    }
}
