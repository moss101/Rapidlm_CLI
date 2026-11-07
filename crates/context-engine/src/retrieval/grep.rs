//! Ripgrep-compatible exact/regex search over a walked repository.
//!
//! Uses the ignore-aware walker (no symlink follow) and an in-process regex
//! engine so lexical search does not require a host `rg` binary. Results are
//! bounded; cancellation and invalid patterns fail closed.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::{RepoId, RepoPath};
use regex::{Regex, RegexBuilder};

use crate::ingest::content::{ContentClass, ContentHash, ContentLimits, load_file_candidate};
use crate::ingest::walk::{
    FileCandidate, IndexEligibility, MetadataOnlyReason, WalkError, WalkLimits, walk_repo,
};
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
    ignore_case: bool,
}

/// One matching line. Body is truncated, never a full file dump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrepHit {
    repo_id: RepoId,
    path: RepoPath,
    line: u32,
    text: String,
    truncated: bool,
    content_hash: ContentHash,
}

/// Bounded results plus whether visible input was omitted by traversal or
/// text-size limits. Ignored paths and binary files are deliberate exclusions.
pub struct GrepReport {
    hits: Vec<GrepHit>,
    incomplete: bool,
}

impl GrepReport {
    pub fn into_parts(self) -> (Vec<GrepHit>, bool) {
        (self.hits, self.incomplete)
    }
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

    pub fn walk_limits(mut self, value: WalkLimits) -> Self {
        self.walk = value;
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
            ignore_case: false,
        }
    }

    pub fn regex(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            mode: GrepMode::Regex,
            prefix: None,
            ignore_case: false,
        }
    }

    pub fn prefix(mut self, path: RepoPath) -> Self {
        self.prefix = Some(path);
        self
    }

    pub fn case_insensitive(mut self, enabled: bool) -> Self {
        self.ignore_case = enabled;
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

    /// Hash of the original file bytes used to produce this hit.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Whether the source line exceeded the configured display byte limit.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

/// Search `repo` for `query`. Missing files and binary/metadata-only paths skip.
pub fn grep_search(
    repo: &RepoSpec,
    query: &GrepQuery,
    limits: &GrepLimits,
) -> Result<Vec<GrepHit>, GrepError> {
    grep_search_filtered(repo, query, limits, &mut |_| true)
}

/// Apply the caller's narrower path selection before loading file contents.
/// Ignore rules, confinement, cancellation and matching stay in this engine.
pub fn grep_search_filtered(
    repo: &RepoSpec,
    query: &GrepQuery,
    limits: &GrepLimits,
    keep: &mut impl FnMut(&RepoPath) -> bool,
) -> Result<Vec<GrepHit>, GrepError> {
    Ok(grep_search_filtered_report(repo, query, limits, keep)?.hits)
}

/// Search like [`grep_search_filtered`] while reporting skipped visible input.
pub fn grep_search_filtered_report(
    repo: &RepoSpec,
    query: &GrepQuery,
    limits: &GrepLimits,
    keep: &mut impl FnMut(&RepoPath) -> bool,
) -> Result<GrepReport, GrepError> {
    let started = Instant::now();
    check_bounds(limits, started)?;
    if query.pattern.is_empty() || query.pattern.len() > limits.max_pattern_bytes {
        return Err(GrepError::InvalidPattern);
    }
    if limits.max_hits == 0 || limits.max_line_bytes == 0 {
        return Err(GrepError::InvalidPolicy);
    }
    let prepared = PreparedGrep::compile(query, limits, started)?;
    // The filesystem walker does not promise an iteration order. Retain the
    // lexicographically earliest bounded set while scanning all candidates;
    // sorting an arbitrary early prefix would make offset paging skip hits.
    let mut hits: BTreeMap<(RepoPath, u32), GrepHit> = BTreeMap::new();
    let mut incomplete = false;
    let mut walker = walk_repo(repo, &limits.walk, &limits.cancel);
    for (i, item) in walker.by_ref().enumerate() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(limits, started)?;
        }
        let candidate = item?;
        if !prefix_matches(query.prefix.as_ref(), candidate.path()) || !keep(candidate.path()) {
            continue;
        }
        if candidate.metadata().eligibility()
            == IndexEligibility::MetadataOnly(MetadataOnlyReason::Oversized)
        {
            incomplete = true;
            continue;
        }
        // A full set can only improve if a candidate sorts at or before its
        // last retained path. The threshold only moves toward earlier paths.
        if hits.len() >= limits.max_hits as usize
            && hits
                .last_key_value()
                .is_some_and(|((path, _), _)| candidate.path() > path)
        {
            continue;
        }
        incomplete |= search_candidate(repo, &candidate, &prepared, &mut hits)?;
    }
    incomplete |= walker.was_incomplete();
    Ok(GrepReport {
        hits: hits.into_values().collect(),
        incomplete,
    })
}

impl<'a> PreparedGrep<'a> {
    fn compile(
        query: &'a GrepQuery,
        limits: &'a GrepLimits,
        started: Instant,
    ) -> Result<Self, GrepError> {
        check_bounds(limits, started)?;
        match query.mode {
            GrepMode::Exact if !query.ignore_case => Ok(Self {
                exact: Some(query.pattern.as_str()),
                regex: None,
                limits,
                started,
            }),
            GrepMode::Exact | GrepMode::Regex => {
                let pattern = if query.mode == GrepMode::Exact {
                    regex::escape(&query.pattern)
                } else {
                    query.pattern.clone()
                };
                let regex = RegexBuilder::new(&pattern)
                    .case_insensitive(query.ignore_case)
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
    hits: &mut BTreeMap<(RepoPath, u32), GrepHit>,
) -> Result<bool, GrepError> {
    check_bounds(prepared.limits, prepared.started)?;
    let loaded = load_file_candidate(
        repo.root().as_path(),
        candidate,
        &prepared.limits.content,
        &prepared.limits.cancel,
    )
    .map_err(map_content)?;
    if !matches!(loaded.class(), ContentClass::Text { .. }) {
        return Ok(matches!(
            loaded.class(),
            ContentClass::MetadataOnly(MetadataOnlyReason::Oversized)
        ));
    }
    let Some(text) = loaded.text() else {
        return Ok(false);
    };
    for (idx, line) in text.lines().enumerate() {
        if idx.is_multiple_of(CANCEL_STRIDE) {
            check_bounds(prepared.limits, prepared.started)?;
        }
        if !prepared.matches(line) {
            continue;
        }
        let line_no = u32::try_from(idx.saturating_add(1)).unwrap_or(u32::MAX);
        let key = (candidate.path().clone(), line_no);
        if hits.len() >= prepared.limits.max_hits as usize
            && hits.last_key_value().is_some_and(|(last, _)| &key >= last)
        {
            // Lines in one file are visited in order, so no later line can
            // improve this bounded prefix either.
            break;
        }
        hits.insert(
            key,
            GrepHit {
                repo_id: candidate.repo_id(),
                path: candidate.path().clone(),
                line: line_no,
                text: truncate_line(line, prepared.limits.max_line_bytes),
                truncated: line.len() > prepared.limits.max_line_bytes,
                content_hash: loaded.content_hash(),
            },
        );
        if hits.len() > prepared.limits.max_hits as usize {
            hits.pop_last();
        }
    }
    Ok(false)
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
    fn report_marks_depth_and_selected_oversized_files_as_incomplete() {
        let ws = TempWorkspace::new();
        ws.write_file("core/visible.txt", b"needle\n");
        ws.write_file("core/deep/sub/hidden.txt", b"needle\n");
        let repo = repo(&ws);
        let shallow = GrepLimits::new().walk_limits(WalkLimits::new().max_depth(1));
        let (hits, incomplete) =
            grep_search_filtered_report(&repo, &GrepQuery::exact("needle"), &shallow, &mut |_| {
                true
            })
            .unwrap()
            .into_parts();
        assert_eq!(hits.len(), 1);
        assert!(incomplete, "a depth-limited directory must be reported");
        ws.write_file("core/large.txt", &vec![b'x'; 1_048_577]);
        let (hits, incomplete) = grep_search_filtered_report(
            &repo,
            &GrepQuery::exact("needle"),
            &GrepLimits::new(),
            &mut |_| true,
        )
        .unwrap()
        .into_parts();
        assert_eq!(hits.len(), 2);
        assert!(incomplete, "an oversized selected file must be reported");
        let (_, incomplete) = grep_search_filtered_report(
            &repo,
            &GrepQuery::exact("needle"),
            &GrepLimits::new(),
            &mut |path| path.as_str() == "visible.txt",
        )
        .unwrap()
        .into_parts();
        assert!(
            !incomplete,
            "an oversized file outside the filter is excluded intentionally"
        );
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
        assert_eq!(
            hits[0].content_hash(),
            ContentHash::from_bytes(b"fn grep_marker() {}\nfn other() {}\n")
        );
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
    fn capped_search_keeps_the_same_lexical_prefix_regardless_of_creation_order() {
        for order in [["z", "m", "a"], ["a", "m", "z"], ["m", "z", "a"]] {
            let ws = TempWorkspace::new();
            for name in order {
                let contents = if name == "a" {
                    "TOKEN first\nTOKEN second\n"
                } else {
                    "TOKEN\n"
                };
                ws.write_file(&format!("core/{name}.rs"), contents.as_bytes());
            }
            let spec = repo(&ws);
            for cap in [1, 2, 3] {
                let hits = grep_search(
                    &spec,
                    &GrepQuery::exact("TOKEN"),
                    &GrepLimits::new().max_hits(cap),
                )
                .unwrap();
                let actual = hits
                    .iter()
                    .map(|hit| (hit.path().as_str(), hit.line()))
                    .collect::<Vec<_>>();
                let expected = [("a.rs", 1), ("a.rs", 2), ("m.rs", 1)];
                assert_eq!(actual, expected[..cap as usize]);
            }
        }
    }

    #[test]
    fn oversized_hit_lines_are_marked_for_readback_without_splitting_unicode() {
        let ws = TempWorkspace::new();
        let line = format!("TOKEN{}", "🐈".repeat(200));
        ws.write_file("core/a.rs", line.as_bytes());
        let hits = grep_search(&repo(&ws), &GrepQuery::exact("TOKEN"), &GrepLimits::new()).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].is_truncated());
        assert!(line.starts_with(hits[0].text()));
        assert!(hits[0].text().len() <= DEFAULT_MAX_LINE_BYTES);
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
