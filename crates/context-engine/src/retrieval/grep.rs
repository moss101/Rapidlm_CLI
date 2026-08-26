//! Ripgrep-compatible exact/regex search over a walked repository.
//!
//! Uses the ignore-aware walker (no symlink follow) and an in-process regex
//! engine so lexical search does not require a host `rg` binary. Results are
//! bounded; cancellation and invalid patterns fail closed.

use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{RepoId, RepoPath};
use regex::{Regex, RegexBuilder};

use crate::ingest::content::{ContentClass, ContentLimits, load_file_candidate};
use crate::ingest::walk::{FileCandidate, WalkError, WalkLimits, walk_repo};
use crate::repo_manifest::{CancellationToken, RepoSpec};

/// Default wall-clock budget for one grep.
pub const DEFAULT_GREP_TIMEOUT: Duration = Duration::from_secs(2);

/// Default hit cap.
pub const DEFAULT_MAX_GREP_HITS: u32 = 64;

/// Default UTF-8 pattern bound.
pub const DEFAULT_MAX_PATTERN_BYTES: usize = 256;

/// Default per-line byte cap returned with a hit.
pub const DEFAULT_MAX_LINE_BYTES: usize = 512;

const CANCEL_STRIDE: usize = 16;

/// Exact substring vs regex. Exact is never compiled as a regex wildcard.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GrepMode {
    Exact,
    Regex,
}

/// Bounds for one [`grep_search`].
#[derive(Clone, Debug)]
pub struct GrepLimits {
    max_hits: u32,
    max_pattern_bytes: usize,
    max_line_bytes: usize,
    timeout: Duration,
    walk: WalkLimits,
    content: ContentLimits,
    cancel: CancellationToken,
}

/// One lexical query against a single repo.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrepQuery {
    pattern: String,
    mode: GrepMode,
    prefix: Option<RepoPath>,
}

/// One matching line. Body is truncated, never a full file dump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrepHit {
    repo_id: RepoId,
    path: RepoPath,
    line: u32,
    text: String,
}

/// Typed grep failure. Display never echoes the pattern or host paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrepError {
    Cancelled,
    Timeout,
    InvalidPattern,
    InvalidPolicy,
    UnknownRepo,
    Walk,
    Io,
}

/// Compiled pattern plus remaining budget.
struct PreparedGrep<'a> {
    exact: Option<&'a str>,
    regex: Option<Regex>,
    limits: &'a GrepLimits,
    started: Instant,
}

impl GrepLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_hits(mut self, value: u32) -> Self {
        self.max_hits = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }
}

impl Default for GrepLimits {
    fn default() -> Self {
        Self {
            max_hits: DEFAULT_MAX_GREP_HITS,
            max_pattern_bytes: DEFAULT_MAX_PATTERN_BYTES,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            timeout: DEFAULT_GREP_TIMEOUT,
            walk: WalkLimits::default(),
            content: ContentLimits::default(),
            cancel: CancellationToken::new(),
        }
    }
}

impl GrepQuery {
    pub fn exact(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            mode: GrepMode::Exact,
            prefix: None,
        }
    }

    pub fn regex(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            mode: GrepMode::Regex,
            prefix: None,
        }
    }

    pub fn prefix(mut self, path: RepoPath) -> Self {
        self.prefix = Some(path);
        self
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn mode(&self) -> GrepMode {
        self.mode
    }
}

impl GrepHit {
    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn line(&self) -> u32 {
        self.line
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Search `repo` for `query`. Missing files and binary/metadata-only paths skip.
pub fn grep_search(
    repo: &RepoSpec,
    query: &GrepQuery,
    limits: &GrepLimits,
) -> Result<Vec<GrepHit>, GrepError> {
    let started = Instant::now();
    check_bounds(limits, started)?;
    if query.pattern.is_empty() || query.pattern.len() > limits.max_pattern_bytes {
        return Err(GrepError::InvalidPattern);
    }
    if limits.max_hits == 0 || limits.max_line_bytes == 0 {
        return Err(GrepError::InvalidPolicy);
    }
    let prepared = PreparedGrep::compile(query, limits, started)?;
    let mut hits = Vec::new();
    for (i, item) in walk_repo(repo, &limits.walk, &limits.cancel).enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(limits, started)?;
        }
        let candidate = item?;
        if !prefix_matches(query.prefix.as_ref(), candidate.path()) {
            continue;
        }
        search_candidate(repo, &candidate, &prepared, &mut hits)?;
        if hits.len() as u32 >= limits.max_hits {
            break;
        }
    }
    Ok(hits)
}

impl<'a> PreparedGrep<'a> {
    fn compile(
        query: &'a GrepQuery,
        limits: &'a GrepLimits,
        started: Instant,
    ) -> Result<Self, GrepError> {
        check_bounds(limits, started)?;
        match query.mode {
            GrepMode::Exact => Ok(Self {
                exact: Some(query.pattern.as_str()),
                regex: None,
                limits,
                started,
            }),
            GrepMode::Regex => {
                let regex = RegexBuilder::new(&query.pattern)
                    .size_limit(1_048_576)
                    .dfa_size_limit(1_048_576)
                    .build()
                    .map_err(|_| GrepError::InvalidPattern)?;
                Ok(Self {
                    exact: None,
                    regex: Some(regex),
                    limits,
                    started,
                })
            }
        }
    }

    fn matches(&self, line: &str) -> bool {
        if let Some(exact) = self.exact {
            line.contains(exact)
        } else if let Some(regex) = self.regex.as_ref() {
            regex.is_match(line)
        } else {
            false
        }
    }
}

fn search_candidate(
    repo: &RepoSpec,
    candidate: &FileCandidate,
    prepared: &PreparedGrep<'_>,
    hits: &mut Vec<GrepHit>,
) -> Result<(), GrepError> {
    check_bounds(prepared.limits, prepared.started)?;
    let loaded = load_file_candidate(
        repo.root().as_path(),
        candidate,
        &prepared.limits.content,
        &prepared.limits.cancel,
    )
    .map_err(map_content)?;
    if !matches!(loaded.class(), ContentClass::Text { .. }) {
        return Ok(());
    }
    let Some(text) = loaded.text() else {
        return Ok(());
    };
    for (idx, line) in text.lines().enumerate() {
        if idx.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(prepared.limits, prepared.started)?;
        }
        if !prepared.matches(line) {
            continue;
        }
        let line_no = u32::try_from(idx.saturating_add(1)).unwrap_or(u32::MAX);
        hits.push(GrepHit {
            repo_id: candidate.repo_id(),
            path: candidate.path().clone(),
            line: line_no,
            text: truncate_line(line, prepared.limits.max_line_bytes),
        });
        if hits.len() as u32 >= prepared.limits.max_hits {
            break;
        }
    }
    Ok(())
}

fn prefix_matches(prefix: Option<&RepoPath>, path: &RepoPath) -> bool {
    match prefix {
        None => true,
        Some(prefix) => {
            let p = prefix.as_str();
            let q = path.as_str();
            q == p || q.starts_with(&format!("{p}/"))
        }
    }
}

fn truncate_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

fn check_bounds(limits: &GrepLimits, started: Instant) -> Result<(), GrepError> {
    if limits.cancel.is_cancelled() {
        return Err(GrepError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(GrepError::Timeout);
    }
    Ok(())
}

fn map_content(err: crate::ingest::content::ContentError) -> GrepError {
    match err {
        crate::ingest::content::ContentError::Cancelled => GrepError::Cancelled,
        crate::ingest::content::ContentError::Io
        | crate::ingest::content::ContentError::NotRegularFile
        | crate::ingest::content::ContentError::PathEscapesRoot => GrepError::Io,
    }
}

impl fmt::Display for GrepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Timeout => f.write_str("timeout"),
            Self::InvalidPattern => f.write_str("invalid_pattern"),
            Self::InvalidPolicy => f.write_str("invalid_policy"),
            Self::UnknownRepo => f.write_str("unknown_repo"),
            Self::Walk => f.write_str("walk"),
            Self::Io => f.write_str("io"),
        }
    }
}

impl Error for GrepError {}

impl From<WalkError> for GrepError {
    fn from(value: WalkError) -> Self {
        match value {
            WalkError::Cancelled => Self::Cancelled,
            WalkError::UnknownRepo => Self::UnknownRepo,
            _ => Self::Walk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_manifest::WorkspaceManifest;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-context-grep-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("temp");
            Self { path }
        }

        fn write_file(&self, rel: &str, bytes: &[u8]) {
            let path = self.path.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(path, bytes).expect("write");
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn repo(ws: &TempWorkspace) -> RepoSpec {
        ws.write_file("core/.keep", b"");
        let src =
            "schema = 1\n[[repos]]\nalias = \"core\"\nroot = \"core\"\nmode = \"read_write\"\n";
        WorkspaceManifest::parse(src, &ws.path, &CancellationToken::new())
            .expect("manifest")
            .repo_by_alias("core")
            .expect("repo")
            .clone()
    }

    #[test]
    fn exact_search_finds_literal_and_not_regex_metacharacters() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/a.rs", b"fn grep_marker() {}\nfn other() {}\n");
        ws.write_file("core/src/b.rs", b"let x = grep_marker_plus;\n");
        let hits = grep_search(
            &repo(&ws),
            &GrepQuery::exact("grep_marker"),
            &GrepLimits::new(),
        )
        .expect("exact");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.text().contains("grep_marker")));
        let dots = grep_search(
            &repo(&ws),
            &GrepQuery::exact("grep.marker"),
            &GrepLimits::new(),
        )
        .expect("literal dots");
        assert!(dots.is_empty());
    }

    #[test]
    fn regex_search_matches_pattern_and_rejects_invalid() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn alpha_one() {}\nfn alpha_two() {}\n");
        let hits = grep_search(
            &repo(&ws),
            &GrepQuery::regex(r"fn alpha_[a-z]+"),
            &GrepLimits::new(),
        )
        .expect("regex");
        assert_eq!(hits.len(), 2);
        let err = grep_search(
            &repo(&ws),
            &GrepQuery::regex("(unclosed"),
            &GrepLimits::new(),
        )
        .expect_err("invalid");
        assert_eq!(err, GrepError::InvalidPattern);
        assert_eq!(err.to_string(), "invalid_pattern");
        assert!(!err.to_string().contains("unclosed"));
    }

    #[test]
    fn prefix_scope_and_hit_cap_are_enforced() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/a.rs", b"TOKEN\n");
        ws.write_file("core/other/b.rs", b"TOKEN\n");
        let prefix = RepoPath::parse("src").expect("prefix");
        let hits = grep_search(
            &repo(&ws),
            &GrepQuery::exact("TOKEN").prefix(prefix),
            &GrepLimits::new().max_hits(1),
        )
        .expect("scoped");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path().as_str().starts_with("src/"));
    }

    #[test]
    fn cancelled_and_empty_pattern_fail_closed() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/a.rs", b"fn x() {}\n");
        let spec = repo(&ws);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = grep_search(
            &spec,
            &GrepQuery::exact("fn"),
            &GrepLimits::new().cancellation(cancel),
        )
        .expect_err("cancelled");
        assert_eq!(err, GrepError::Cancelled);
        let err = grep_search(&spec, &GrepQuery::exact(""), &GrepLimits::new()).expect_err("empty");
        assert_eq!(err, GrepError::InvalidPattern);
    }
}
