//! Ignore-aware streaming inventory of indexable repository files.
//!
//! The walker is an iterator: directory state is a depth stack, not a collected
//! path list. `.gitignore` and `.rapidlmignore` exclude paths before a candidate
//! is yielded. Symlinks are never followed, so they cannot escape a repo root.
//! Size and binary limits do not drop a visible file; they mark it metadata-only.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, ReadDir};
use std::io::Read;
use std::path::{Path, PathBuf};

use protocol::{RepoId, RepoPath};

use crate::repo_manifest::{CancellationToken, RepoSpec, WorkspaceManifest};

/// Default per-file byte cap for full-text eligibility.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1_048_576;

/// Bounded prefix inspected for a NUL byte (binary hint).
pub const DEFAULT_BINARY_PROBE_BYTES: usize = 8192;

/// Maximum UTF-8 bytes accepted in one ignore file.
pub const MAX_IGNORE_FILE_BYTES: usize = 256 * 1024;

/// Maximum repository-relative directory depth descended.
pub const DEFAULT_MAX_DEPTH: usize = 256;

const CANCEL_STRIDE: u32 = 16;
const GIT_DIR: &str = ".git";
const RAPIDLM_DIR: &str = ".rapidlm";
const GITIGNORE_NAME: &str = ".gitignore";
const RAPIDLMIGNORE_NAME: &str = ".rapidlmignore";
const GIT_EXCLUDE: &str = ".git/info/exclude";

/// Resource bounds for one walk. Deeper/larger inputs are skipped or classified,
/// not buffered into RAM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalkLimits {
    max_file_bytes: u64,
    binary_probe_bytes: usize,
    max_ignore_bytes: usize,
    max_depth: usize,
}

/// Which repositories from a manifest participate in a walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoScope {
    All,
    Only(RepoId),
}

/// Why a candidate is not eligible for full-text indexing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataOnlyReason {
    Oversized,
    Binary,
}

/// Indexing class derived from size and a bounded binary probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexEligibility {
    FullText,
    MetadataOnly(MetadataOnlyReason),
}

/// Host metadata captured without loading file contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    size: u64,
    eligibility: IndexEligibility,
}

/// One indexable (or metadata-only) regular file inside a repo scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileCandidate {
    repo_id: RepoId,
    path: RepoPath,
    metadata: FileMetadata,
}

/// Typed walk failure. Display never echoes host or repository paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkError {
    Cancelled,
    Io,
    IgnoreTooLarge,
    UnknownRepo,
    RootInvalid,
}

/// Streaming iterator over [`FileCandidate`] values for one or more repos.
pub struct FileWalker<'a> {
    repos: Vec<&'a RepoSpec>,
    next_repo: usize,
    current: Option<RepoWalk>,
    limits: &'a WalkLimits,
    cancel: &'a CancellationToken,
    steps: u32,
    pending: Option<WalkError>,
}

struct RepoWalk {
    repo_id: RepoId,
    root: PathBuf,
    stack: Vec<DirFrame>,
    rules: Vec<IgnoreRule>,
}

struct DirFrame {
    iter: ReadDir,
    rel: String,
    depth: usize,
    rules_len: usize,
}

#[derive(Clone, Debug)]
struct IgnoreRule {
    negated: bool,
    dir_only: bool,
    anchored: bool,
    base: String,
    glob: String,
}

impl WalkLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_file_bytes(mut self, value: u64) -> Self {
        self.max_file_bytes = value;
        self
    }

    pub fn binary_probe_bytes(mut self, value: usize) -> Self {
        self.binary_probe_bytes = value;
        self
    }

    pub fn max_ignore_bytes(mut self, value: usize) -> Self {
        self.max_ignore_bytes = value;
        self
    }

    pub fn max_depth(mut self, value: usize) -> Self {
        self.max_depth = value;
        self
    }

    pub fn max_file_bytes_value(&self) -> u64 {
        self.max_file_bytes
    }

    pub fn binary_probe_bytes_value(&self) -> usize {
        self.binary_probe_bytes
    }

    pub fn max_ignore_bytes_value(&self) -> usize {
        self.max_ignore_bytes
    }

    pub fn max_depth_value(&self) -> usize {
        self.max_depth
    }
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            binary_probe_bytes: DEFAULT_BINARY_PROBE_BYTES,
            max_ignore_bytes: MAX_IGNORE_FILE_BYTES,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

impl FileMetadata {
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn eligibility(&self) -> IndexEligibility {
        self.eligibility
    }

    pub fn is_full_text(&self) -> bool {
        matches!(self.eligibility, IndexEligibility::FullText)
    }
}

impl FileCandidate {
    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn metadata(&self) -> &FileMetadata {
        &self.metadata
    }
}

impl WalkError {
    fn from_io(_: std::io::Error) -> Self {
        Self::Io
    }
}

/// Walk every repository selected by `scope`.
pub fn walk_manifest<'a>(
    manifest: &'a WorkspaceManifest,
    scope: RepoScope,
    limits: &'a WalkLimits,
    cancel: &'a CancellationToken,
) -> FileWalker<'a> {
    let (repos, pending) = match scope {
        RepoScope::All => (manifest.repos().iter().collect(), None),
        RepoScope::Only(id) => match manifest.repo_by_id(id) {
            Some(repo) => (vec![repo], None),
            None => (Vec::new(), Some(WalkError::UnknownRepo)),
        },
    };
    FileWalker {
        repos,
        next_repo: 0,
        current: None,
        limits,
        cancel,
        steps: 0,
        pending,
    }
}

/// Walk a single already-validated repository specification.
pub fn walk_repo<'a>(
    repo: &'a RepoSpec,
    limits: &'a WalkLimits,
    cancel: &'a CancellationToken,
) -> FileWalker<'a> {
    FileWalker {
        repos: vec![repo],
        next_repo: 0,
        current: None,
        limits,
        cancel,
        steps: 0,
        pending: None,
    }
}

impl Iterator for FileWalker<'_> {
    type Item = Result<FileCandidate, WalkError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(err) = self.pending.take() {
            return Some(Err(err));
        }
        loop {
            if let Err(err) = self.check_cancel() {
                return Some(Err(err));
            }
            if self.current.is_none() {
                if !self.open_next_repo() {
                    return None;
                }
                if let Some(err) = self.pending.take() {
                    return Some(Err(err));
                }
            }
            match self.step_current() {
                Step::Yield(item) => return Some(item),
                Step::Continue => {}
                Step::RepoDone => self.current = None,
            }
        }
    }
}

enum Step {
    Yield(Result<FileCandidate, WalkError>),
    Continue,
    RepoDone,
}

impl FileWalker<'_> {
    fn check_cancel(&mut self) -> Result<(), WalkError> {
        self.steps = self.steps.wrapping_add(1);
        if (self.steps == 1 || self.steps.is_multiple_of(CANCEL_STRIDE))
            && self.cancel.is_cancelled() {
                return Err(WalkError::Cancelled);
            }
        Ok(())
    }

    fn open_next_repo(&mut self) -> bool {
        // Every branch below returns, so this opens at most one repo per call;
        // failures are surfaced once via `pending` (fail-closed), not skipped.
        if self.next_repo < self.repos.len() {
            let spec = self.repos[self.next_repo];
            self.next_repo += 1;
            match start_repo(spec, self.limits) {
                Ok(walk) => {
                    self.current = Some(walk);
                    return true;
                }
                Err(err) => {
                    self.pending = Some(err);
                    self.current = None;
                    return true;
                }
            }
        }
        false
    }

    fn step_current(&mut self) -> Step {
        let Some(walk) = self.current.as_mut() else {
            return Step::RepoDone;
        };
        let next = {
            let Some(frame) = walk.stack.last_mut() else {
                return Step::RepoDone;
            };
            match frame.iter.next() {
                None => FrameNext::Pop(frame.rules_len),
                Some(Err(_)) => FrameNext::Io,
                Some(Ok(entry)) => FrameNext::Entry {
                    parent_rel: frame.rel.clone(),
                    parent_depth: frame.depth,
                    entry,
                },
            }
        };
        match next {
            FrameNext::Pop(rules_len) => {
                walk.rules.truncate(rules_len);
                walk.stack.pop();
                if walk.stack.is_empty() {
                    Step::RepoDone
                } else {
                    Step::Continue
                }
            }
            FrameNext::Io => Step::Yield(Err(WalkError::Io)),
            FrameNext::Entry {
                parent_rel,
                parent_depth,
                entry,
            } => step_entry(walk, self.limits, parent_rel, parent_depth, entry),
        }
    }
}

// Private, hot-path iterator state; boxing the large variant would allocate
// per entry, so the size skew is accepted.
#[allow(clippy::large_enum_variant)]
enum FrameNext {
    Pop(usize),
    Io,
    Entry {
        parent_rel: String,
        parent_depth: usize,
        entry: std::fs::DirEntry,
    },
}

fn step_entry(
    walk: &mut RepoWalk,
    limits: &WalkLimits,
    parent_rel: String,
    parent_depth: usize,
    entry: std::fs::DirEntry,
) -> Step {
    let name = match entry.file_name().to_str() {
        Some(name) if !name.is_empty() && name != "." && name != ".." => name.to_owned(),
        _ => return Step::Continue,
    };
    if name.contains('\0') {
        return Step::Continue;
    }
    let rel = join_rel(&parent_rel, &name);
    let path = match RepoPath::parse(&rel) {
        Ok(path) => path,
        Err(_) => return Step::Continue,
    };
    let abs = abs_in(&walk.root, &rel);
    let meta = match fs::symlink_metadata(&abs) {
        Ok(meta) => meta,
        Err(_) => return Step::Continue,
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Step::Continue;
    }
    if file_type.is_dir() {
        return push_dir(walk, limits, rel, parent_depth + 1);
    }
    if !file_type.is_file() {
        return Step::Continue;
    }
    if is_builtin_ignored(&rel) || is_ignored(&walk.rules, &rel, false) {
        return Step::Continue;
    }
    let size = meta.len();
    let eligibility = classify_file(&abs, size, limits);
    Step::Yield(Ok(FileCandidate {
        repo_id: walk.repo_id,
        path,
        metadata: FileMetadata { size, eligibility },
    }))
}

fn start_repo(spec: &RepoSpec, limits: &WalkLimits) -> Result<RepoWalk, WalkError> {
    let root = spec.root().as_path();
    let meta = fs::symlink_metadata(root).map_err(WalkError::from_io)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(WalkError::RootInvalid);
    }
    let canon = fs::canonicalize(root).map_err(WalkError::from_io)?;
    if canon != root {
        return Err(WalkError::RootInvalid);
    }
    let iter = fs::read_dir(&canon).map_err(WalkError::from_io)?;
    let mut walk = RepoWalk {
        repo_id: spec.id(),
        root: canon,
        stack: vec![DirFrame {
            iter,
            rel: String::new(),
            depth: 0,
            rules_len: 0,
        }],
        rules: Vec::new(),
    };
    append_ignore_file(&mut walk, "", GIT_EXCLUDE, limits)?;
    append_ignore_file(&mut walk, "", GITIGNORE_NAME, limits)?;
    append_ignore_file(&mut walk, "", RAPIDLMIGNORE_NAME, limits)?;
    Ok(walk)
}

fn push_dir(walk: &mut RepoWalk, limits: &WalkLimits, rel: String, depth: usize) -> Step {
    if is_builtin_ignored(&rel) || is_ignored(&walk.rules, &rel, true) {
        return Step::Continue;
    }
    if depth > limits.max_depth {
        return Step::Continue;
    }
    let abs = abs_in(&walk.root, &rel);
    let canon = match fs::canonicalize(&abs) {
        Ok(canon) => canon,
        Err(_) => return Step::Continue,
    };
    if !path_within(&canon, &walk.root) {
        return Step::Continue;
    }
    let iter = match fs::read_dir(&canon) {
        Ok(iter) => iter,
        Err(_) => return Step::Yield(Err(WalkError::Io)),
    };
    let rules_len = walk.rules.len();
    if let Err(err) = load_dir_ignores(walk, &rel, limits) {
        return Step::Yield(Err(err));
    }
    walk.stack.push(DirFrame {
        iter,
        rel,
        depth,
        rules_len,
    });
    Step::Continue
}

fn load_dir_ignores(walk: &mut RepoWalk, rel: &str, limits: &WalkLimits) -> Result<(), WalkError> {
    let gitignore = format!("{rel}/{GITIGNORE_NAME}");
    let rapidlmignore = format!("{rel}/{RAPIDLMIGNORE_NAME}");
    append_ignore_file(walk, rel, &gitignore, limits)?;
    append_ignore_file(walk, rel, &rapidlmignore, limits)?;
    Ok(())
}

fn append_ignore_file(
    walk: &mut RepoWalk,
    base: &str,
    rel_file: &str,
    limits: &WalkLimits,
) -> Result<(), WalkError> {
    let abs = abs_in(&walk.root, rel_file);
    let Some(src) = read_ignore_source(&abs, &walk.root, limits.max_ignore_bytes)? else {
        return Ok(());
    };
    parse_ignore_rules(&src, base, &mut walk.rules);
    Ok(())
}

fn read_ignore_source(
    path: &Path,
    root: &Path,
    max_bytes: usize,
) -> Result<Option<String>, WalkError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if meta.file_type().is_symlink() {
        let resolved = match fs::canonicalize(path) {
            Ok(resolved) => resolved,
            Err(_) => return Ok(None),
        };
        if !path_within(&resolved, root) {
            return Ok(None);
        }
        if !resolved.is_file() {
            return Ok(None);
        }
    } else if !meta.file_type().is_file() {
        return Ok(None);
    }
    if meta.len() > max_bytes as u64 {
        return Err(WalkError::IgnoreTooLarge);
    }
    let bytes = fs::read(path).map_err(WalkError::from_io)?;
    if bytes.len() > max_bytes {
        return Err(WalkError::IgnoreTooLarge);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn classify_file(path: &Path, size: u64, limits: &WalkLimits) -> IndexEligibility {
    if size > limits.max_file_bytes {
        return IndexEligibility::MetadataOnly(MetadataOnlyReason::Oversized);
    }
    if file_looks_binary(path, limits.binary_probe_bytes) {
        return IndexEligibility::MetadataOnly(MetadataOnlyReason::Binary);
    }
    IndexEligibility::FullText
}

fn file_looks_binary(path: &Path, probe_bytes: usize) -> bool {
    if probe_bytes == 0 {
        return false;
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return true,
    };
    let mut buf = vec![0u8; probe_bytes];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return true,
    };
    buf[..n].contains(&0)
}

fn is_builtin_ignored(rel: &str) -> bool {
    first_component(rel) == Some(GIT_DIR) || first_component(rel) == Some(RAPIDLM_DIR)
}

fn first_component(rel: &str) -> Option<&str> {
    rel.split('/').next().filter(|c| !c.is_empty())
}

fn is_ignored(rules: &[IgnoreRule], rel: &str, is_dir: bool) -> bool {
    let mut ignored = false;
    for rule in rules {
        if rule.matches(rel, is_dir) {
            ignored = !rule.negated;
        }
    }
    ignored
}

impl IgnoreRule {
    fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let Some(path) = strip_base(rel, &self.base) else {
            return false;
        };
        if path.is_empty() {
            return false;
        }
        if self.anchored {
            glob_match_path(&self.glob, path)
        } else {
            unanchored_match(&self.glob, path)
        }
    }
}

fn strip_base<'a>(rel: &'a str, base: &str) -> Option<&'a str> {
    if base.is_empty() {
        return Some(rel);
    }
    let rest = rel.strip_prefix(base)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

fn unanchored_match(glob: &str, path: &str) -> bool {
    if glob_match_path(glob, path) {
        return true;
    }
    for (idx, ch) in path.char_indices() {
        if ch == '/' && glob_match_path(glob, &path[idx + 1..]) {
            return true;
        }
    }
    false
}

fn parse_ignore_rules(src: &str, base: &str, out: &mut Vec<IgnoreRule>) {
    let body = src.strip_prefix('\u{feff}').unwrap_or(src);
    for raw in body.lines() {
        if let Some(rule) = parse_ignore_line(raw, base) {
            out.push(rule);
        }
    }
}

fn parse_ignore_line(raw: &str, base: &str) -> Option<IgnoreRule> {
    let line = raw.strip_suffix('\r').unwrap_or(raw);
    let line = trim_unescaped_trailing_space(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, rest) = if let Some(rest) = line.strip_prefix('!') {
        (true, rest)
    } else {
        (false, line)
    };
    if rest.is_empty() {
        return None;
    }
    let dir_only = rest.ends_with('/');
    let mut glob = rest.trim_end_matches('/');
    if glob.is_empty() {
        return None;
    }
    let anchored = glob.starts_with('/') || glob[..glob.len().saturating_sub(1)].contains('/');
    if let Some(stripped) = glob.strip_prefix('/') {
        glob = stripped;
    }
    if glob.is_empty() {
        return None;
    }
    Some(IgnoreRule {
        negated,
        dir_only,
        anchored,
        base: base.to_owned(),
        glob: glob.to_owned(),
    })
}

fn trim_unescaped_trailing_space(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b' ' {
        let escaped = end >= 2 && bytes[end - 2] == b'\\' && !is_escaped(bytes, end - 2);
        if escaped {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

fn is_escaped(bytes: &[u8], idx: usize) -> bool {
    let mut slashes = 0;
    let mut i = idx;
    while i > 0 && bytes[i - 1] == b'\\' {
        slashes += 1;
        i -= 1;
    }
    slashes % 2 == 1
}

fn glob_match_path(pat: &str, path: &str) -> bool {
    let pat_parts: Vec<&str> = if pat.is_empty() {
        Vec::new()
    } else {
        pat.split('/').collect()
    };
    let path_parts: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    glob_match_parts(&pat_parts, &path_parts)
}

fn glob_match_parts(pat: &[&str], text: &[&str]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    if pat[0] == "**" {
        return glob_match_parts(&pat[1..], text)
            || (!text.is_empty() && glob_match_parts(pat, &text[1..]));
    }
    !text.is_empty() && match_component(pat[0], text[0]) && glob_match_parts(&pat[1..], &text[1..])
}

fn match_component(pat: &str, name: &str) -> bool {
    match_chars(
        &pat.chars().collect::<Vec<_>>(),
        &name.chars().collect::<Vec<_>>(),
    )
}

fn match_chars(pat: &[char], name: &[char]) -> bool {
    let Some((&pc, prest)) = pat.split_first() else {
        return name.is_empty();
    };
    match pc {
        '*' => match_chars(prest, name) || (!name.is_empty() && match_chars(pat, &name[1..])),
        '?' => !name.is_empty() && match_chars(prest, &name[1..]),
        '\\' => match prest.split_first() {
            Some((&lit, rest)) => {
                !name.is_empty() && name[0] == lit && match_chars(rest, &name[1..])
            }
            None => name.is_empty(),
        },
        '[' => match parse_class(prest) {
            Some((class, rest)) => {
                !name.is_empty() && class.matches(name[0]) && match_chars(rest, &name[1..])
            }
            None => !name.is_empty() && name[0] == '[' && match_chars(prest, &name[1..]),
        },
        _ => !name.is_empty() && name[0] == pc && match_chars(prest, &name[1..]),
    }
}

struct CharClass {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl CharClass {
    fn matches(&self, ch: char) -> bool {
        let hit = self.ranges.iter().any(|&(a, b)| ch >= a && ch <= b);
        if self.negated { !hit } else { hit }
    }
}

fn parse_class(pat: &[char]) -> Option<(CharClass, &[char])> {
    let mut i = 0;
    if i >= pat.len() {
        return None;
    }
    let negated = matches!(pat[i], '!' | '^');
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    while i < pat.len() {
        if pat[i] == ']' && !first {
            return Some((CharClass { negated, ranges }, &pat[i + 1..]));
        }
        first = false;
        let start = pat[i];
        i += 1;
        if i + 1 < pat.len() && pat[i] == '-' && pat[i + 1] != ']' {
            let end = pat[i + 1];
            i += 2;
            if end >= start {
                ranges.push((start, end));
            }
        } else {
            ranges.push((start, start));
        }
    }
    None
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn abs_in(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    if !rel.is_empty() {
        for part in rel.split('/') {
            out.push(part);
        }
    }
    out
}

fn path_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "context file walk cancelled",
            Self::Io => "context file walk failed due to an I/O error",
            Self::IgnoreTooLarge => "ignore file exceeds the configured size limit",
            Self::UnknownRepo => "unknown repository in walk scope",
            Self::RootInvalid => "repository root is not a usable directory",
        })
    }
}

impl Error for WalkError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_manifest::WorkspaceManifest;
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
                "rapidlm-context-walk-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("temp workspace");
            Self { path }
        }

        fn mkdir(&self, rel: &str) -> PathBuf {
            let path = self.path.join(rel);
            fs::create_dir_all(&path).expect("mkdir");
            path
        }

        fn write_file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.path.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(&path, bytes).expect("write");
            path
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn parse_manifest(ws: &TempWorkspace, repos: &[(&str, &str)]) -> WorkspaceManifest {
        let mut src = String::from("schema = 1\n");
        for (alias, root) in repos {
            ws.mkdir(root);
            src.push_str(&format!(
                "[[repos]]\nalias = \"{alias}\"\nroot = \"{root}\"\nmode = \"read_write\"\n"
            ));
        }
        WorkspaceManifest::parse(&src, &ws.path, &CancellationToken::new()).expect("manifest")
    }

    fn paths_of(walk: FileWalker<'_>) -> Vec<String> {
        let mut out = Vec::new();
        for item in walk {
            let candidate = item.expect("walk item");
            out.push(candidate.path().as_str().to_owned());
        }
        out.sort();
        out
    }

    fn walk_one<'a>(
        manifest: &'a WorkspaceManifest,
        limits: &'a WalkLimits,
        cancel: &'a CancellationToken,
    ) -> FileWalker<'a> {
        walk_manifest(manifest, RepoScope::All, limits, cancel)
    }

    #[test]
    fn gitignore_and_rapidlmignore_exclude_paths() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"fn main() {}");
        ws.write_file("core/src/secret.rs", b"secret");
        ws.write_file("core/build/out.rs", b"generated");
        ws.write_file("core/notes.log", b"log");
        ws.write_file("core/private/hidden.rs", b"no");
        ws.write_file("core/.gitignore", b"secret.rs\nbuild/\n*.log\n");
        ws.write_file("core/.rapidlmignore", b"private/\n");
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        let paths = paths_of(walk_one(&manifest, &limits, &cancel));
        assert_eq!(
            paths,
            vec![
                ".gitignore".to_owned(),
                ".rapidlmignore".to_owned(),
                "src/lib.rs".to_owned()
            ]
        );
        assert!(!paths.iter().any(|p| p.contains("secret")
            || p.contains("build")
            || p.contains("private")
            || p.ends_with(".log")));
    }

    #[test]
    fn nested_gitignore_applies_in_its_directory() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/keep.rs", b"keep");
        ws.write_file("core/src/skip.rs", b"skip");
        ws.write_file("core/src/.gitignore", b"skip.rs\n");
        ws.write_file("core/skip.rs", b"root-skip-not-ignored");
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        let paths = paths_of(walk_one(&manifest, &limits, &cancel));
        assert!(paths.contains(&"src/keep.rs".to_owned()));
        assert!(paths.contains(&"skip.rs".to_owned()));
        assert!(!paths.contains(&"src/skip.rs".to_owned()));
    }

    #[test]
    fn negation_reincludes_file_in_same_directory() {
        let ws = TempWorkspace::new();
        ws.write_file("core/keep.rs", b"keep");
        ws.write_file("core/drop.rs", b"drop");
        ws.write_file("core/.gitignore", b"*.rs\n!keep.rs\n");
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        let paths = paths_of(walk_one(&manifest, &limits, &cancel));
        assert!(paths.contains(&"keep.rs".to_owned()));
        assert!(!paths.contains(&"drop.rs".to_owned()));
    }

    #[test]
    fn builtin_git_and_rapidlm_dirs_are_never_walked() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"ok");
        ws.write_file("core/.git/objects/pack", b"blob");
        ws.write_file("core/.rapidlm/config.toml", b"x = 1\n");
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        let paths = paths_of(walk_one(&manifest, &limits, &cancel));
        assert_eq!(paths, vec!["src/lib.rs".to_owned()]);
    }

    #[test]
    fn oversized_and_binary_files_are_metadata_only() {
        let ws = TempWorkspace::new();
        ws.write_file("core/small.rs", b"fn x() {}");
        ws.write_file("core/huge.rs", &[b'a'; 64]);
        ws.write_file("core/blob.bin", &[0x00, 0x01, 0x02, 0x00]);
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::new().max_file_bytes(16).binary_probe_bytes(8);
        let cancel = CancellationToken::new();
        let mut seen_small = false;
        let mut seen_huge = false;
        let mut seen_blob = false;
        for item in walk_one(&manifest, &limits, &cancel) {
            let candidate = item.expect("candidate");
            match candidate.path().as_str() {
                "small.rs" => {
                    seen_small = true;
                    assert!(candidate.metadata().is_full_text());
                    assert_eq!(candidate.metadata().size(), 9);
                }
                "huge.rs" => {
                    seen_huge = true;
                    assert_eq!(
                        candidate.metadata().eligibility(),
                        IndexEligibility::MetadataOnly(MetadataOnlyReason::Oversized)
                    );
                    assert_eq!(candidate.metadata().size(), 64);
                }
                "blob.bin" => {
                    seen_blob = true;
                    assert_eq!(
                        candidate.metadata().eligibility(),
                        IndexEligibility::MetadataOnly(MetadataOnlyReason::Binary)
                    );
                }
                other => panic!("unexpected path {other}"),
            }
        }
        assert!(seen_small && seen_huge && seen_blob);
    }

    #[test]
    fn repo_scope_does_not_cross_repository_roots() {
        let ws = TempWorkspace::new();
        ws.write_file("alpha/a.rs", b"a");
        ws.write_file("beta/b.rs", b"b");
        let manifest = parse_manifest(&ws, &[("alpha", "alpha"), ("beta", "beta")]);
        let alpha = manifest.repo_by_alias("alpha").expect("alpha");
        let beta = manifest.repo_by_alias("beta").expect("beta");
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        let alpha_paths = paths_of(walk_manifest(
            &manifest,
            RepoScope::Only(alpha.id()),
            &limits,
            &cancel,
        ));
        let beta_paths = paths_of(walk_repo(beta, &limits, &cancel));
        assert_eq!(alpha_paths, vec!["a.rs".to_owned()]);
        assert_eq!(beta_paths, vec!["b.rs".to_owned()]);
        let unknown = walk_manifest(&manifest, RepoScope::Only(RepoId::new()), &limits, &cancel)
            .collect::<Vec<_>>();
        assert_eq!(unknown.len(), 1);
        assert_eq!(
            unknown[0].as_ref().err().copied(),
            Some(WalkError::UnknownRepo)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_symlink_files_are_excluded() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"ok");
        let outside = ws.write_file("outside/secret.rs", b"secret");
        let leak = ws.path.join("core/leak.rs");
        std::os::unix::fs::symlink(&outside, &leak).expect("file symlink");
        let escape_dir = ws.path.join("core/escape");
        std::os::unix::fs::symlink(ws.path.join("outside"), &escape_dir).expect("dir symlink");
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        let paths = paths_of(walk_one(&manifest, &limits, &cancel));
        assert_eq!(paths, vec!["src/lib.rs".to_owned()]);
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("secret") || p.contains("leak") || p.contains("escape"))
        );
    }

    #[test]
    fn cancelled_walk_fails_closed() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"ok");
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = walk_one(&manifest, &limits, &cancel)
            .next()
            .expect("item")
            .expect_err("cancelled");
        assert_eq!(err, WalkError::Cancelled);
        assert_eq!(err.to_string(), "context file walk cancelled");
        assert!(!err.to_string().contains("core"));
    }

    #[test]
    fn one_hundred_thousand_files_stream_without_collecting_paths() {
        let ws = TempWorkspace::new();
        const N: usize = 100_000;
        const DIRS: usize = 100;
        const PER_DIR: usize = N / DIRS;
        for dir in 0..DIRS {
            let dir_path = ws.path.join("core").join(format!("d{dir}"));
            fs::create_dir_all(&dir_path).expect("bulk dir");
            for file in 0..PER_DIR {
                fs::write(dir_path.join(format!("f{file}.rs")), b"x").expect("bulk file");
            }
        }
        let manifest = parse_manifest(&ws, &[("core", "core")]);
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        let mut count = 0usize;
        let mut last_size = 0u64;
        for item in walk_one(&manifest, &limits, &cancel) {
            let candidate = item.expect("candidate");
            last_size = candidate.metadata().size();
            count += 1;
        }
        assert_eq!(count, N);
        assert_eq!(last_size, 1);
    }

    #[test]
    fn glob_helpers_cover_gitignore_shapes() {
        assert!(glob_match_path("*.rs", "lib.rs"));
        assert!(!glob_match_path("*.rs", "src/lib.rs"));
        assert!(unanchored_match("*.rs", "src/lib.rs"));
        assert!(glob_match_path("src/*.rs", "src/lib.rs"));
        assert!(!glob_match_path("src/*.rs", "src/n/lib.rs"));
        assert!(glob_match_path("**/*.rs", "src/n/lib.rs"));
        assert!(glob_match_path("foo/**", "foo/a/b"));
        assert!(match_component("f?o", "foo"));
        assert!(match_component("f[ae]o", "feo"));
        assert!(!match_component("f[ae]o", "fio"));
        assert!(match_component("f[!a]o", "foo"));
    }

    #[test]
    fn error_display_does_not_echo_paths() {
        for err in [
            WalkError::Cancelled,
            WalkError::Io,
            WalkError::IgnoreTooLarge,
            WalkError::UnknownRepo,
            WalkError::RootInvalid,
        ] {
            let shown = err.to_string();
            assert!(!shown.contains('\\'));
            assert!(!shown.contains(".."));
            assert!(!shown.contains("secret"));
        }
    }
}
