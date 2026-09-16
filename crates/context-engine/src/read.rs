//! Bounded `repo.read` with independent line/byte/per-line/token ceilings.
//!
//! Truncation always returns a structured continuation cursor and the reason.
//! Empty results are never used as a stand-in for “there is more.”

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use protocol::{RepoId, RepoPath};

use crate::ingest::content::{ContentHash, ContentLimits, load_candidate};
use crate::repo_manifest::CancellationToken;
use crate::token_estimate::{TokenEstimateLimits, TokenEstimator, TokenizerFamily};

/// Default line window (1-indexed count).
pub const DEFAULT_MAX_READ_LINES: u32 = 400;

/// Default UTF-8 byte cap for the returned slice.
pub const DEFAULT_MAX_READ_BYTES: usize = 64 * 1024;

/// Default per-line character cap. Longer lines are clamped, not dropped.
pub const DEFAULT_MAX_CHARS_PER_LINE: usize = 512;

/// Default estimated-token cap for one read.
pub const DEFAULT_MAX_READ_TOKENS: u32 = 4_096;

/// Default wall-clock budget.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(2);

const CANCEL_STRIDE: usize = 16;

/// Independent ceilings applied to one read.
#[derive(Clone, Debug)]
pub struct ReadLimits {
    max_lines: u32,
    max_bytes: usize,
    max_chars_per_line: usize,
    max_tokens: u32,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Requested window. `cursor` continues a previous truncated read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadRequest {
    repo_id: RepoId,
    path: RepoPath,
    start_line: u32,
    line_window: u32,
    cursor: Option<ReadCursor>,
}

/// Resume point after a bounded read. `next_line` is 1-indexed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadCursor {
    path: RepoPath,
    next_line: u32,
    next_byte: u32,
    reason: TruncationReason,
}

/// Why the returned slice stopped before the end of the file.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TruncationReason {
    LineWindow,
    MaxBytes,
    MaxLineChars,
    TokenBudget,
}

/// How complete the returned slice is relative to the file/window.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ReadCompleteness {
    Full,
    Partial,
    Clamped,
}

/// Bounded file slice plus optional continuation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadSlice {
    repo_id: RepoId,
    path: RepoPath,
    text: String,
    content_hash: ContentHash,
    start_line: u32,
    end_line: u32,
    start_byte: u32,
    end_byte: u32,
    completeness: ReadCompleteness,
    cursor: Option<ReadCursor>,
    estimated_tokens: u32,
}

/// Typed read failure. Display never echoes paths or file text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    Cancelled,
    Timeout,
    InvalidRequest,
    InvalidPolicy,
    NotFound,
    NotText,
    PathEscapesRoot,
}

impl ReadLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_lines(mut self, value: u32) -> Self {
        self.max_lines = value;
        self
    }

    pub fn max_bytes(mut self, value: usize) -> Self {
        self.max_bytes = value;
        self
    }

    pub fn max_chars_per_line(mut self, value: usize) -> Self {
        self.max_chars_per_line = value;
        self
    }

    pub fn max_tokens(mut self, value: u32) -> Self {
        self.max_tokens = value;
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

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_READ_LINES,
            max_bytes: DEFAULT_MAX_READ_BYTES,
            max_chars_per_line: DEFAULT_MAX_CHARS_PER_LINE,
            max_tokens: DEFAULT_MAX_READ_TOKENS,
            timeout: DEFAULT_READ_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

impl ReadRequest {
    pub fn new(path: RepoPath) -> Self {
        Self {
            repo_id: RepoId::new(),
            path,
            start_line: 1,
            line_window: DEFAULT_MAX_READ_LINES,
            cursor: None,
        }
    }

    pub fn repo(mut self, repo_id: RepoId) -> Self {
        self.repo_id = repo_id;
        self
    }

    pub fn start_line(mut self, value: u32) -> Self {
        self.start_line = value;
        self
    }

    pub fn line_window(mut self, value: u32) -> Self {
        self.line_window = value;
        self
    }

    pub fn cursor(mut self, cursor: ReadCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }
}

impl ReadCursor {
    pub fn next_line(&self) -> u32 {
        self.next_line
    }

    pub fn next_byte(&self) -> u32 {
        self.next_byte
    }

    pub fn reason(&self) -> TruncationReason {
        self.reason
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }
}

impl ReadSlice {
    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn start_line(&self) -> u32 {
        self.start_line
    }

    pub fn end_line(&self) -> u32 {
        self.end_line
    }

    pub fn start_byte(&self) -> u32 {
        self.start_byte
    }

    pub fn end_byte(&self) -> u32 {
        self.end_byte
    }

    pub fn completeness(&self) -> ReadCompleteness {
        self.completeness
    }

    pub fn cursor(&self) -> Option<&ReadCursor> {
        self.cursor.as_ref()
    }

    pub fn estimated_tokens(&self) -> u32 {
        self.estimated_tokens
    }
}

impl TruncationReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LineWindow => "line_window",
            Self::MaxBytes => "max_bytes",
            Self::MaxLineChars => "max_line_chars",
            Self::TokenBudget => "token_budget",
        }
    }
}

impl ReadCompleteness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
            Self::Clamped => "clamped",
        }
    }
}

impl ReadError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidRequest => "invalid_request",
            Self::InvalidPolicy => "invalid_policy",
            Self::NotFound => "not_found",
            Self::NotText => "not_text",
            Self::PathEscapesRoot => "path_escapes_root",
        }
    }
}

impl fmt::Display for TruncationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ReadCompleteness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ReadError {}

/// Read a repository-relative file under `root` with independent ceilings.
pub fn read_repo(
    root: &Path,
    request: &ReadRequest,
    limits: &ReadLimits,
) -> Result<ReadSlice, ReadError> {
    let started = Instant::now();
    check_ready(limits, started)?;
    validate_limits(limits)?;
    if request.start_line == 0 || request.line_window == 0 {
        return Err(ReadError::InvalidRequest);
    }
    if let Some(cursor) = &request.cursor
        && (cursor.path != request.path || cursor.next_line == 0)
    {
        return Err(ReadError::InvalidRequest);
    }

    let abs = resolve_file(root, &request.path)?;
    check_ready(limits, started)?;
    let bytes = fs::read(&abs).map_err(|_| ReadError::NotFound)?;
    let loaded = load_candidate(
        &request.path,
        &bytes,
        &ContentLimits::new().max_file_bytes(bytes.len() as u64 + 1),
        &limits.cancel,
    )
    .map_err(map_content)?;
    let Some(text) = loaded.text() else {
        return Err(ReadError::NotText);
    };
    let content_hash = loaded.content_hash();

    let start_line = request
        .cursor
        .as_ref()
        .map(|c| c.next_line)
        .unwrap_or(request.start_line);
    slice_text(
        request.repo_id,
        request.path.clone(),
        text,
        content_hash,
        start_line,
        request.line_window,
        limits,
        started,
    )
}

#[allow(clippy::too_many_arguments)]
fn slice_text(
    repo_id: RepoId,
    path: RepoPath,
    text: &str,
    content_hash: ContentHash,
    start_line: u32,
    line_window: u32,
    limits: &ReadLimits,
    started: Instant,
) -> Result<ReadSlice, ReadError> {
    let max_lines = line_window.min(limits.max_lines);
    let mut out = String::new();
    let mut line_no = 0u32;
    let mut start_byte = 0u32;
    let mut end_byte = 0u32;
    let mut end_line = start_line.saturating_sub(1);
    let mut byte_off = 0u32;
    let mut taken = 0u32;
    let mut clamped = false;
    let mut stop = None::<TruncationReason>;
    let mut estimator = TokenEstimator::with_limits(
        TokenEstimateLimits::new()
            .timeout(limits.timeout)
            .cancellation(limits.cancel.clone()),
    );

    for (step, raw_line) in text.split_inclusive('\n').enumerate() {
        if step.is_multiple_of(CANCEL_STRIDE) {
            check_ready(limits, started)?;
        }
        line_no = line_no.saturating_add(1);
        let line_bytes = raw_line.len() as u32;
        if line_no < start_line {
            byte_off = byte_off.saturating_add(line_bytes);
            continue;
        }
        if taken == 0 {
            start_byte = byte_off;
        }
        if taken >= max_lines {
            stop = Some(TruncationReason::LineWindow);
            // A limit that can't admit even the first considered line must
            // still advance the cursor past it, or a caller that follows
            // `next_line` retries the same `start_line` forever.
            if taken == 0 {
                end_line = line_no;
            }
            break;
        }

        let (emitted, line_clamped) = clamp_line(raw_line, limits.max_chars_per_line);
        if line_clamped {
            clamped = true;
        }
        if out.len().saturating_add(emitted.len()) > limits.max_bytes {
            stop = Some(TruncationReason::MaxBytes);
            if taken == 0 {
                end_line = line_no;
            }
            break;
        }
        out.push_str(emitted);
        if line_clamped
            && !emitted.ends_with('\n')
            && raw_line.ends_with('\n')
            && out.len().saturating_add(1) <= limits.max_bytes
        {
            out.push('\n');
        }

        let tokens = estimator
            .estimate(TokenizerFamily::Unknown, &out)
            .map_err(map_token)?;
        if tokens.tokens() > limits.max_tokens {
            while !out.is_empty() && {
                let t = estimator
                    .estimate(TokenizerFamily::Unknown, &out)
                    .map_err(map_token)?;
                t.tokens() > limits.max_tokens
            } {
                out.pop();
            }
            stop = Some(TruncationReason::TokenBudget);
            clamped = true;
            if taken == 0 {
                end_line = line_no;
            }
            break;
        }

        taken = taken.saturating_add(1);
        end_line = line_no;
        byte_off = byte_off.saturating_add(line_bytes);
        end_byte = byte_off;
        if line_clamped {
            stop = Some(TruncationReason::MaxLineChars);
            break;
        }
    }

    let more = stop.is_some() || line_no >= start_line && byte_off < text.len() as u32;
    let completeness = if clamped {
        ReadCompleteness::Clamped
    } else if stop.is_some() || more && taken > 0 {
        ReadCompleteness::Partial
    } else {
        ReadCompleteness::Full
    };
    let cursor = match stop {
        Some(reason)
            if byte_off < text.len() as u32
                || taken < line_no.saturating_sub(start_line.saturating_sub(1)) =>
        {
            Some(ReadCursor {
                path: path.clone(),
                next_line: end_line.saturating_add(1).max(start_line),
                next_byte: end_byte,
                reason,
            })
        }
        Some(reason) if completeness != ReadCompleteness::Full => Some(ReadCursor {
            path: path.clone(),
            next_line: end_line.saturating_add(1).max(start_line),
            next_byte: end_byte,
            reason,
        }),
        _ => None,
    };

    let estimated_tokens = if out.is_empty() {
        0
    } else {
        estimator
            .estimate(TokenizerFamily::Unknown, &out)
            .map_err(map_token)?
            .tokens()
    };

    Ok(ReadSlice {
        repo_id,
        path,
        text: out,
        content_hash,
        start_line,
        end_line: if taken == 0 { start_line } else { end_line },
        start_byte,
        end_byte,
        completeness,
        cursor,
        estimated_tokens,
    })
}

fn clamp_line(line: &str, max_chars: usize) -> (&str, bool) {
    if max_chars == 0 {
        return ("", true);
    }
    let chars = line.chars().count();
    if chars <= max_chars {
        return (line, false);
    }
    let mut end = 0;
    for (i, (off, _)) in line.char_indices().enumerate() {
        if i == max_chars {
            end = off;
            break;
        }
        end = line.len();
    }
    (line.get(..end).unwrap_or(""), true)
}

fn resolve_file(root: &Path, rel: &RepoPath) -> Result<PathBuf, ReadError> {
    let root_meta = fs::symlink_metadata(root).map_err(|_| ReadError::NotFound)?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(ReadError::PathEscapesRoot);
    }
    let root_canon = protocol::host_path::canonicalize(root).map_err(|_| ReadError::NotFound)?;
    let abs = root_canon.join(rel.as_str());
    let meta = fs::symlink_metadata(&abs).map_err(|_| ReadError::NotFound)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(ReadError::NotFound);
    }
    let parent = abs.parent().ok_or(ReadError::PathEscapesRoot)?;
    let parent_canon =
        protocol::host_path::canonicalize(parent).map_err(|_| ReadError::PathEscapesRoot)?;
    if !parent_canon.starts_with(&root_canon) {
        return Err(ReadError::PathEscapesRoot);
    }
    Ok(abs)
}

fn validate_limits(limits: &ReadLimits) -> Result<(), ReadError> {
    if limits.max_lines == 0
        || limits.max_bytes == 0
        || limits.max_chars_per_line == 0
        || limits.max_tokens == 0
    {
        return Err(ReadError::InvalidPolicy);
    }
    Ok(())
}

fn check_ready(limits: &ReadLimits, started: Instant) -> Result<(), ReadError> {
    if limits.cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(ReadError::Timeout);
    }
    Ok(())
}

fn map_content(err: crate::ingest::content::ContentError) -> ReadError {
    match err {
        crate::ingest::content::ContentError::Cancelled => ReadError::Cancelled,
        crate::ingest::content::ContentError::PathEscapesRoot => ReadError::PathEscapesRoot,
        crate::ingest::content::ContentError::NotRegularFile => ReadError::NotFound,
        crate::ingest::content::ContentError::Io => ReadError::NotFound,
    }
}

fn map_token(err: crate::token_estimate::TokenEstimateError) -> ReadError {
    match err {
        crate::token_estimate::TokenEstimateError::Cancelled => ReadError::Cancelled,
        crate::token_estimate::TokenEstimateError::Timeout => ReadError::Timeout,
        crate::token_estimate::TokenEstimateError::TextTooLarge
        | crate::token_estimate::TokenEstimateError::TokenizerFailed => ReadError::InvalidPolicy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rapidlm-context-read-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("temp");
            Self { path }
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.path.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(path, bytes).expect("write");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("path")
    }

    #[test]
    fn full_small_file_has_no_cursor() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", b"fn a() {}\nfn b() {}\n");
        let slice = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")),
            &ReadLimits::new(),
        )
        .expect("read");
        assert_eq!(slice.completeness(), ReadCompleteness::Full);
        assert!(slice.cursor().is_none());
        assert!(slice.text().contains("fn a()"));
        assert_eq!(slice.start_line(), 1);
        assert!(slice.estimated_tokens() >= 1);
    }

    #[test]
    fn line_window_returns_cursor() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", b"l1\nl2\nl3\nl4\n");
        let first = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")).line_window(2),
            &ReadLimits::new().max_lines(2),
        )
        .expect("first");
        assert_eq!(first.completeness(), ReadCompleteness::Partial);
        let cursor = first.cursor().expect("cursor").clone();
        assert_eq!(cursor.reason(), TruncationReason::LineWindow);
        assert_eq!(cursor.next_line(), 3);
        let rest = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")).cursor(cursor),
            &ReadLimits::new(),
        )
        .expect("rest");
        assert!(rest.text().contains("l3"));
        assert!(rest.text().contains("l4"));
    }

    #[test]
    fn byte_ceiling_truncates_with_reason() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", b"aaaaaaaaaa\nbbbbbbbbbb\n");
        let slice = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")),
            &ReadLimits::new().max_bytes(8),
        )
        .expect("read");
        assert!(slice.text().len() <= 8);
        assert_eq!(
            slice.cursor().expect("cursor").reason(),
            TruncationReason::MaxBytes
        );
        assert_ne!(slice.completeness(), ReadCompleteness::Full);
    }

    #[test]
    fn long_line_is_clamped_not_dropped() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", b"abcdefghijklmnopqrstuvwxyz\n");
        let slice = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")),
            &ReadLimits::new().max_chars_per_line(5),
        )
        .expect("read");
        assert_eq!(slice.completeness(), ReadCompleteness::Clamped);
        assert_eq!(
            slice.cursor().expect("cursor").reason(),
            TruncationReason::MaxLineChars
        );
        assert!(!slice.text().is_empty());
        assert!(slice.text().chars().count() <= 6);
    }

    #[test]
    fn missing_file_is_not_found() {
        let dir = TempDir::new();
        assert_eq!(
            read_repo(
                &dir.path,
                &ReadRequest::new(path("missing.rs")),
                &ReadLimits::new()
            ),
            Err(ReadError::NotFound)
        );
    }

    #[test]
    fn cancelled_read_fails_closed() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", b"fn x() {}\n");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            read_repo(
                &dir.path,
                &ReadRequest::new(path("src/lib.rs")),
                &ReadLimits::new().cancellation(cancel)
            ),
            Err(ReadError::Cancelled)
        );
    }

    #[test]
    fn error_display_is_safe() {
        assert_eq!(ReadError::NotFound.to_string(), "not_found");
        assert!(!ReadError::PathEscapesRoot.to_string().contains('/'));
    }

    #[test]
    fn max_bytes_smaller_than_the_first_line_still_advances_the_cursor() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", b"hello\nworld\n");
        let first = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")),
            &ReadLimits::new().max_bytes(1),
        )
        .expect("read");
        assert!(first.text().is_empty());
        let cursor = first.cursor().expect("cursor").clone();
        assert_eq!(cursor.reason(), TruncationReason::MaxBytes);
        assert_eq!(
            cursor.next_line(),
            2,
            "a limit that can't admit even the first line must still skip past it"
        );

        let second = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")).cursor(cursor),
            &ReadLimits::new().max_bytes(1),
        )
        .expect("read");
        assert_ne!(
            second.cursor().map(|c| c.next_line()),
            Some(2),
            "following the cursor must make forward progress, not repeat the same line forever"
        );
    }

    #[test]
    fn max_tokens_smaller_than_the_first_line_still_advances_the_cursor() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", b"a very normal first line of code\nsecond\n");
        let first = read_repo(
            &dir.path,
            &ReadRequest::new(path("src/lib.rs")),
            &ReadLimits::new().max_tokens(1),
        )
        .expect("read");
        let cursor = first.cursor().expect("cursor").clone();
        assert_eq!(cursor.reason(), TruncationReason::TokenBudget);
        assert_eq!(
            cursor.next_line(),
            2,
            "a token limit that can't admit even the first line must still skip past it"
        );
    }
}
