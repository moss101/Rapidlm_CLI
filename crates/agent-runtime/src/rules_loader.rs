//! Deterministic AGENTS hierarchy loader (prompt-runtime §3, P5-030).
//!
//! Discovers `AGENTS.md` files from the repository root down to the current
//! task/file scope, reads them in root→nearer order, and returns a bounded
//! [`RulesBundle`]. Project content is lower authority than system/developer/
//! security policy: the prompt compiler places it in a trusted-project slot the
//! model cannot use to override higher authority. This is not ad-hoc Markdown
//! concatenation — locators, ordering, bounds and path confinement are typed.
//!
//! Not every markdown file is an instruction: only `AGENTS.md` at a directory
//! boundary is loaded.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::model::CancellationToken;

/// The only file considered part of the instruction hierarchy.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// Maximum AGENTS.md files accepted in one hierarchy.
pub const MAX_AGENTS_FILES: usize = 16;

/// Maximum combined UTF-8 bytes across the loaded hierarchy.
pub const MAX_AGENTS_BYTES: usize = 256 * 1024;

/// One loaded AGENTS hierarchy file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleEntry {
    path: String,
    text: String,
}

impl RuleEntry {
    fn new(path: String, text: String) -> Self {
        Self { path, text }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Bounded, ordered result of a hierarchy load.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RulesBundle {
    entries: Vec<RuleEntry>,
}

impl RulesBundle {
    fn new(entries: Vec<RuleEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[RuleEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Composed project-instruction text, root-first, each file delimited with
    /// its repo-relative locator so precedence is explicit.
    pub fn composed(&self) -> String {
        let mut out = String::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str("# AGENTS/");
            out.push_str(&entry.path);
            out.push('\n');
            out.push_str(&entry.text);
        }
        out
    }
}

/// Typed rules-loader failure. Display never includes file contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RulesError {
    Cancelled,
    InvalidRoot,
    PathEscape,
    Unreadable,
    Malformed,
    BoundExceeded,
    Io,
}

impl RulesError {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "rules loader cancelled",
            Self::InvalidRoot => "rules root is not a directory",
            Self::PathEscape => "task scope escapes the repository root",
            Self::Unreadable => "an AGENTS.md file could not be read",
            Self::Malformed => "an AGENTS.md file is not valid UTF-8",
            Self::BoundExceeded => "AGENTS hierarchy exceeds a documented bound",
            Self::Io => "rules loader I/O failed",
        }
    }
}

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for RulesError {}

/// Load the AGENTS hierarchy for `scope` under `root`.
///
/// Ordering is root-first, then every nearer directory up to the scope's parent
/// directory (task/file scope). Sibling directories are never pulled in. A
/// missing `AGENTS.md` is skipped; an unreadable or non-UTF-8 file fails closed.
/// Scope and root are canonicalized; a scope outside the root is a `PathEscape`.
pub fn load_agents(
    root: &Path,
    scope: &Path,
    cancel: &CancellationToken,
) -> Result<RulesBundle, RulesError> {
    if cancel.is_cancelled() {
        return Err(RulesError::Cancelled);
    }
    let root = canonical_dir(root)?;
    // The scope may be a task file that does not exist yet, so walk directories:
    // the scope directory is the scope path itself when it is a directory,
    // otherwise its parent.
    let scope_dir = resolve_scope_dir(scope)?;
    if !scope_dir.starts_with(&root) {
        return Err(RulesError::PathEscape);
    }

    // Collect directories root-first down to the scope directory (inclusive).
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cursor = scope_dir;
    loop {
        dirs.push(cursor.clone());
        if cursor == root {
            break;
        }
        let Some(parent) = cursor.parent() else {
            return Err(RulesError::PathEscape);
        };
        if parent == cursor || !parent.starts_with(&root) {
            return Err(RulesError::PathEscape);
        }
        cursor = parent.to_path_buf();
    }
    dirs.reverse();

    let mut entries = Vec::new();
    let mut total = 0usize;
    for dir in dirs {
        if cancel.is_cancelled() {
            return Err(RulesError::Cancelled);
        }
        let agents = dir.join(AGENTS_FILE);
        let metadata = match fs::metadata(&agents) {
            Ok(meta) => meta,
            Err(_) => continue, // missing AGENTS.md is not an error
        };
        if !metadata.is_file() {
            continue;
        }
        let bytes = fs::read(&agents).map_err(|_| RulesError::Unreadable)?;
        let text = String::from_utf8(bytes).map_err(|_| RulesError::Malformed)?;
        total = total
            .checked_add(text.len())
            .ok_or(RulesError::BoundExceeded)?;
        if total > MAX_AGENTS_BYTES || entries.len() >= MAX_AGENTS_FILES {
            return Err(RulesError::BoundExceeded);
        }
        let rel = dir
            .strip_prefix(&root)
            .map_err(|_| RulesError::PathEscape)?
            .to_path_buf();
        let locator = if rel.as_os_str().is_empty() {
            AGENTS_FILE.to_owned()
        } else {
            format!("{}/{}", rel.to_string_lossy(), AGENTS_FILE)
        };
        entries.push(RuleEntry::new(locator, text));
    }
    Ok(RulesBundle::new(entries))
}

fn canonical_dir(path: &Path) -> Result<PathBuf, RulesError> {
    if path.as_os_str().is_empty() {
        return Err(RulesError::InvalidRoot);
    }
    let meta = fs::symlink_metadata(path).map_err(|_| RulesError::InvalidRoot)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(RulesError::InvalidRoot);
    }
    let canonical = fs::canonicalize(path).map_err(|_| RulesError::InvalidRoot)?;
    let again = fs::symlink_metadata(&canonical).map_err(|_| RulesError::InvalidRoot)?;
    if again.file_type().is_symlink() || !again.is_dir() {
        return Err(RulesError::InvalidRoot);
    }
    Ok(canonical)
}

fn resolve_scope_dir(scope: &Path) -> Result<PathBuf, RulesError> {
    if scope.as_os_str().is_empty() {
        return Err(RulesError::PathEscape);
    }
    // A scope that itself is a directory is used directly; otherwise the task
    // file may not exist yet, so use its parent directory.
    if let Ok(canonical) = fs::canonicalize(scope)
        && fs::metadata(&canonical)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    {
        return Ok(canonical);
    }
    let parent = scope.parent().ok_or(RulesError::PathEscape)?;
    let canonical = fs::canonicalize(parent).map_err(|_| RulesError::PathEscape)?;
    if fs::metadata(&canonical)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        Ok(canonical)
    } else {
        Err(RulesError::PathEscape)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn scratch() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join("rapidlm-agents").join(format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(scratch())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn root_only_hierarchy() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.0.join("src")).expect("mkdir");
        fs::write(fx.0.join(AGENTS_FILE), "root rules").expect("root");
        let bundle = load_agents(&fx.0, &fx.0.join("src/lib.rs"), &cancel()).expect("load");
        assert_eq!(bundle.len(), 1);
        assert_eq!(bundle.entries()[0].path(), "AGENTS.md");
        assert_eq!(bundle.entries()[0].text(), "root rules");
    }

    #[test]
    fn root_then_nested_ordering_and_sibling_isolation() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.0.join("src/sub")).expect("mkdir");
        fs::create_dir_all(fx.0.join("src/other")).expect("mkdir");
        fs::write(fx.0.join(AGENTS_FILE), "root").expect("root");
        fs::write(fx.0.join("src/AGENTS.md"), "src").expect("src");
        fs::write(fx.0.join("src/other/AGENTS.md"), "other").expect("other");
        // Scope under src/sub must NOT include the sibling src/other hierarchy.
        let bundle = load_agents(&fx.0, &fx.0.join("src/sub/a.rs"), &cancel()).expect("load");
        assert_eq!(bundle.len(), 2);
        assert_eq!(bundle.entries()[0].path(), "AGENTS.md");
        assert_eq!(bundle.entries()[1].path(), "src/AGENTS.md");
        assert!(!bundle.composed().contains("other"));
        assert_eq!(bundle.entries()[0].text(), "root");
        assert_eq!(bundle.entries()[1].text(), "src");
    }

    #[test]
    fn missing_agents_is_empty_and_non_utf8_fails_closed() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.0.join("src")).expect("mkdir");
        let empty = load_agents(&fx.0, &fx.0.join("src/a.rs"), &cancel()).expect("empty");
        assert!(empty.is_empty());

        let bad = Fixture::new();
        fs::create_dir_all(bad.0.join("src")).expect("mkdir");
        fs::write(bad.0.join(AGENTS_FILE), [0xff, 0xfe]).expect("bad");
        let err = load_agents(&bad.0, &bad.0.join("src/a.rs"), &cancel()).expect_err("malformed");
        assert_eq!(err, RulesError::Malformed);
    }

    #[test]
    fn path_escape_and_cancellation_fail_closed() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.0.join("src")).expect("mkdir");
        // A scope outside the root is rejected.
        let outside = scratch();
        fs::create_dir_all(&outside).expect("outside");
        let err = load_agents(&fx.0, &outside, &cancel()).expect_err("escape");
        assert_eq!(err, RulesError::PathEscape);
        let _ = fs::remove_dir_all(&outside);

        let token = cancel();
        token.cancel();
        assert_eq!(
            load_agents(&fx.0, &fx.0.join("src/a.rs"), &token),
            Err(RulesError::Cancelled)
        );
    }

    #[test]
    fn composed_text_is_root_first_and_locator_tagged() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.0.join("nested")).expect("mkdir");
        fs::write(fx.0.join(AGENTS_FILE), "ROOT").expect("root");
        fs::write(fx.0.join("nested/AGENTS.md"), "NESTED").expect("nested");
        let bundle = load_agents(&fx.0, &fx.0.join("nested/x.rs"), &cancel()).expect("load");
        let text = bundle.composed();
        let root_at = text.find("ROOT").expect("root present");
        let nested_at = text.find("NESTED").expect("nested present");
        assert!(root_at < nested_at, "root must come first");
        assert!(text.contains("# AGENTS/AGENTS.md"));
        assert!(text.contains("# AGENTS/nested/AGENTS.md"));
    }
}
