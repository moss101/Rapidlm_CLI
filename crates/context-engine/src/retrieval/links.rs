//! Extract code links from git diffs, workspace manifests, and build/test output.
//!
//! Paths are parsed as [`RepoPath`] so traversal and absolute forms fail closed.
//! Extraction is in-process (no `git`/`cargo` spawn). Oversized input and
//! cancellation degrade to a typed error rather than a partial silent drop.

use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::RepoPath;
use regex::Regex;

use crate::repo_manifest::{CancellationToken, ManifestError, WorkspaceManifest};

/// Default wall-clock budget for one extraction.
pub const DEFAULT_LINK_TIMEOUT: Duration = Duration::from_secs(1);

/// Default UTF-8 byte cap for a source document.
pub const DEFAULT_MAX_LINK_BYTES: usize = 256 * 1024;

/// Default maximum links returned.
pub const DEFAULT_MAX_LINKS: usize = 256;

const CANCEL_STRIDE: usize = 16;

/// Origin of a recovered code path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LinkKind {
    Git,
    Manifest,
    Build,
    Test,
}

/// One repository-relative path recovered from git/manifest/build/test text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeLink {
    kind: LinkKind,
    path: RepoPath,
    line: Option<u32>,
}

/// Bounds for [`extract_links`].
#[derive(Clone, Debug)]
pub struct LinkLimits {
    max_bytes: usize,
    max_links: usize,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Typed extraction failure. Display never echoes paths or source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkError {
    Cancelled,
    Timeout,
    SourceTooLarge,
    InvalidUtf8,
    InvalidManifest,
}

impl LinkKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Manifest => "manifest",
            Self::Build => "build",
            Self::Test => "test",
        }
    }
}

impl CodeLink {
    pub fn kind(&self) -> LinkKind {
        self.kind
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn line(&self) -> Option<u32> {
        self.line
    }
}

impl LinkLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn max_bytes(mut self, value: usize) -> Self {
        self.max_bytes = value;
        self
    }
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_LINK_BYTES,
            max_links: DEFAULT_MAX_LINKS,
            timeout: DEFAULT_LINK_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

/// Extract links of `kind` from `source`.
pub fn extract_links(
    kind: LinkKind,
    source: &str,
    workspace_root: Option<&std::path::Path>,
    limits: &LinkLimits,
) -> Result<Vec<CodeLink>, LinkError> {
    let started = Instant::now();
    check_bounds(limits, started, source.len())?;
    if source.as_bytes().contains(&0) {
        return Err(LinkError::InvalidUtf8);
    }
    match kind {
        LinkKind::Manifest => extract_manifest(source, workspace_root, limits, started),
        LinkKind::Git => extract_git(source, limits, started),
        LinkKind::Build => {
            extract_line_links(source, LinkKind::Build, rustc_path_line(), limits, started)
        }
        LinkKind::Test => {
            extract_line_links(source, LinkKind::Test, test_path_line(), limits, started)
        }
    }
}

fn extract_manifest(
    source: &str,
    workspace_root: Option<&std::path::Path>,
    limits: &LinkLimits,
    started: Instant,
) -> Result<Vec<CodeLink>, LinkError> {
    check_bounds(limits, started, source.len())?;
    let root = workspace_root.unwrap_or_else(|| std::path::Path::new("."));
    let manifest = WorkspaceManifest::parse(source, root, &limits.cancel).map_err(map_manifest)?;
    let mut out = Vec::new();
    for (i, repo) in manifest.repos().iter().enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(limits, started, source.len())?;
        }
        if out.len() >= limits.max_links {
            break;
        }
        let rel = repo.root().as_path().file_name().and_then(|n| n.to_str());
        let Some(rel) = rel else {
            continue;
        };
        if let Ok(path) = RepoPath::parse(rel) {
            out.push(CodeLink {
                kind: LinkKind::Manifest,
                path,
                line: None,
            });
        }
    }
    Ok(out)
}

fn extract_git(
    source: &str,
    limits: &LinkLimits,
    started: Instant,
) -> Result<Vec<CodeLink>, LinkError> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (i, line) in source.lines().enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(limits, started, source.len())?;
        }
        if out.len() >= limits.max_links {
            break;
        }
        let candidate = git_path(line);
        let Some(raw) = candidate else {
            continue;
        };
        if !seen.insert(raw.to_owned()) {
            continue;
        }
        if let Ok(path) = RepoPath::parse(raw) {
            out.push(CodeLink {
                kind: LinkKind::Git,
                path,
                line: None,
            });
        }
    }
    Ok(out)
}

fn extract_line_links(
    source: &str,
    kind: LinkKind,
    pattern: &Regex,
    limits: &LinkLimits,
    started: Instant,
) -> Result<Vec<CodeLink>, LinkError> {
    let mut out = Vec::new();
    for (i, caps) in pattern.captures_iter(source).enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(limits, started, source.len())?;
        }
        if out.len() >= limits.max_links {
            break;
        }
        let raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let line = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
        let Ok(path) = RepoPath::parse(raw) else {
            continue;
        };
        out.push(CodeLink { kind, path, line });
    }
    Ok(out)
}

fn git_path(line: &str) -> Option<&str> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("+++ b/") {
        return Some(rest.split_whitespace().next().unwrap_or(rest));
    }
    if let Some(rest) = line.strip_prefix("--- a/") {
        return Some(rest.split_whitespace().next().unwrap_or(rest));
    }
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let mut parts = rest.split_whitespace();
        let _a = parts.next();
        if let Some(b) = parts.next() {
            return b.strip_prefix("b/");
        }
    }
    None
}

fn rustc_path_line() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*-->\s+([^:\s]+):(\d+)").expect("rustc pattern"))
}

fn test_path_line() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)panicked at ([^:\s]+):(\d+)").expect("test pattern"))
}

fn check_bounds(limits: &LinkLimits, started: Instant, len: usize) -> Result<(), LinkError> {
    if limits.cancel.is_cancelled() {
        return Err(LinkError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(LinkError::Timeout);
    }
    if len > limits.max_bytes {
        return Err(LinkError::SourceTooLarge);
    }
    Ok(())
}

fn map_manifest(err: ManifestError) -> LinkError {
    match err {
        ManifestError::Cancelled => LinkError::Cancelled,
        _ => LinkError::InvalidManifest,
    }
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Timeout => f.write_str("timeout"),
            Self::SourceTooLarge => f.write_str("source_too_large"),
            Self::InvalidUtf8 => f.write_str("invalid_utf8"),
            Self::InvalidManifest => f.write_str("invalid_manifest"),
        }
    }
}

impl Error for LinkError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_diff_extracts_repo_paths_and_rejects_traversal() {
        let src = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
+++ b/../secret.rs
";
        let links = extract_links(LinkKind::Git, src, None, &LinkLimits::new()).expect("git");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path().as_str(), "src/lib.rs");
        assert_eq!(links[0].kind(), LinkKind::Git);
        assert!(links[0].line().is_none());
    }

    #[test]
    fn rustc_and_test_output_capture_path_and_line() {
        let build = "error[E0308]: mismatched types\n --> crates/kernel/src/lib.rs:12:5\n";
        let links = extract_links(LinkKind::Build, build, None, &LinkLimits::new()).expect("build");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path().as_str(), "crates/kernel/src/lib.rs");
        assert_eq!(links[0].line(), Some(12));
        assert_eq!(links[0].kind(), LinkKind::Build);

        let test = "thread 'foo' panicked at src/ingest/pipeline.rs:88:9:\n";
        let links = extract_links(LinkKind::Test, test, None, &LinkLimits::new()).expect("test");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path().as_str(), "src/ingest/pipeline.rs");
        assert_eq!(links[0].line(), Some(88));
        assert_eq!(links[0].kind(), LinkKind::Test);
    }

    #[test]
    fn manifest_lists_repo_aliases_as_links() {
        let path =
            std::env::temp_dir().join(format!("rapidlm-link-manifest-{}", std::process::id()));
        std::fs::create_dir_all(path.join("core")).expect("tmp repo");
        let src =
            "schema = 1\n[[repos]]\nalias = \"core\"\nroot = \"core\"\nmode = \"read_write\"\n";
        let links = extract_links(LinkKind::Manifest, src, Some(&path), &LinkLimits::new())
            .expect("manifest");
        let _ = std::fs::remove_dir_all(&path);
        assert!(
            links
                .iter()
                .any(|l| l.kind() == LinkKind::Manifest && l.path().as_str() == "core")
        );
    }

    #[test]
    fn cancelled_and_oversized_fail_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = extract_links(
            LinkKind::Git,
            "+++ b/src/lib.rs\n",
            None,
            &LinkLimits::new().cancellation(cancel),
        )
        .expect_err("cancelled");
        assert_eq!(err, LinkError::Cancelled);
        assert_eq!(err.to_string(), "cancelled");

        let err = extract_links(
            LinkKind::Git,
            "+++ b/src/lib.rs\n",
            None,
            &LinkLimits::new().max_bytes(4),
        )
        .expect_err("bound");
        assert_eq!(err, LinkError::SourceTooLarge);
    }
}
