//! `@path` references in a prompt.
//!
//! Typing `@src/lib.rs` in a prompt attaches that file's contents to the turn:
//! the model is given the file alongside the prompt, so it need not spend a
//! tool call finding it. `@src/lib.rs:40-80` (or `#L40-L80`) attaches only
//! those lines, `@"a file.txt"` names a path with spaces, and `@src/` attaches
//! a directory listing.
//!
//! The prompt itself is not rewritten: the ledger records what the person
//! typed, and the attached text travels as separate context.
//!
//! A reference is the person naming a file, so it is honoured without asking
//! the model's permission, but not without limits:
//!
//! - the path must stay inside the project, after links are resolved;
//! - a file that looks like it holds a secret is refused, not attached;
//! - binary files are refused, large ones are cut at a stated size;
//! - a text that is not a path to anything is left alone, so `@alice` in a
//!   sentence costs nothing and an e-mail address is never a reference.
//!
//! The same module completes the token under the cursor (`complete`), ranking
//! the project's files by how well they match what has been typed.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Most references honoured in one prompt.
pub const MAX_MENTIONS: usize = 16;

/// Largest slice of one file attached; more is cut, and the cut is stated.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// Most bytes attached across one prompt's references.
pub const MAX_TOTAL_BYTES: usize = 192 * 1024;

/// Most of a file read to find a requested line range.
const MAX_RANGE_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Longest project-relative path attached (the context locator is bounded).
const MAX_PATH_BYTES: usize = 400;

/// Most entries listed for a directory.
const MAX_DIR_ENTRIES: usize = 120;

/// Bytes inspected to decide a file is binary.
const BINARY_PROBE: usize = 8 * 1024;

/// Most candidates `complete` returns.
pub const MAX_CANDIDATES: usize = 8;

/// Most project files considered when completing.
const MAX_INDEXED_FILES: usize = 50_000;

/// Directories skipped when no ignore rules are available.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
];

/// An `@` reference found in a prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mention {
    /// Byte offset of the `@`.
    pub start: usize,
    /// Byte offset just past the last character of the reference.
    pub end: usize,
    /// The path as written, without any line range.
    pub target: String,
    /// First and last line asked for, 1-based and inclusive.
    pub lines: Option<(usize, usize)>,
}

/// Characters a reference may be directly preceded by.
fn opens_mention(previous: Option<char>) -> bool {
    previous.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\'' | '`'))
}

/// Punctuation that ends a sentence rather than a path.
fn is_trailing_punctuation(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '`'
    )
}

/// The `@` references in `text`, at most [`MAX_MENTIONS`].
#[must_use]
pub fn mentions(text: &str) -> Vec<Mention> {
    let mut found = Vec::new();
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if c == '@' && opens_mention(previous) && found.len() < MAX_MENTIONS {
            let rest = &text[index + 1..];
            if let Some((target_text, consumed)) = take_target(rest)
                && let Some(mention) = build_mention(index, target_text, consumed)
            {
                // Resume after the reference so its own `@` is not seen twice.
                let resume = index + 1 + consumed;
                while chars.peek().is_some_and(|(i, _)| *i < resume) {
                    chars.next();
                }
                previous = text[..resume].chars().next_back();
                found.push(mention);
                continue;
            }
        }
        previous = Some(c);
    }
    found
}

/// The written target after an `@`, and how many bytes it used.
fn take_target(rest: &str) -> Option<(String, usize)> {
    if let Some(quoted) = rest.strip_prefix('"') {
        let close = quoted.find(['"', '\n'])?;
        if quoted.as_bytes()[close] != b'"' {
            return None;
        }
        return Some((quoted[..close].to_owned(), close + 2));
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let mut word = &rest[..end];
    while let Some(last) = word.chars().next_back() {
        if is_trailing_punctuation(last) {
            word = &word[..word.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    (!word.is_empty()).then(|| (word.to_owned(), word.len()))
}

fn build_mention(start: usize, written: String, consumed: usize) -> Option<Mention> {
    let (target, lines) = split_range(&written);
    if target.is_empty() || target.starts_with('@') || target.chars().any(char::is_control) {
        return None;
    }
    Some(Mention {
        start,
        end: start + 1 + consumed,
        target: target.to_owned(),
        lines,
    })
}

/// `src/a.rs:10-20`, `src/a.rs#L10-L20` and `src/a.rs:10` into path and range.
fn split_range(written: &str) -> (&str, Option<(usize, usize)>) {
    let parse = |text: &str| -> Option<(usize, usize)> {
        let text = text.trim_start_matches('L');
        let (first, last) = match text.split_once('-') {
            Some((a, b)) => (a, b.trim_start_matches('L')),
            None => (text, text),
        };
        let first: usize = first.parse().ok()?;
        let last: usize = last.parse().ok()?;
        (first >= 1 && last >= first).then_some((first, last))
    };
    for separator in ["#", ":"] {
        if let Some((path, range)) = written.rsplit_once(separator)
            && !path.is_empty()
            && let Some(lines) = parse(range)
        {
            return (path, Some(lines));
        }
    }
    (written, None)
}

/// Why a reference was not attached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skip {
    /// The reference as written.
    pub target: String,
    /// Why, in words for the person who typed it.
    pub reason: String,
}

/// A file or directory ready to hand to the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    /// The path relative to the project, with `/` separators.
    pub path: String,
    /// The text attached for the model.
    pub text: String,
    /// What the person is told was attached.
    pub summary: String,
}

/// What a prompt's references resolved to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Attachments {
    pub attached: Vec<Reference>,
    pub skipped: Vec<Skip>,
}

/// Resolve every reference in `text` against the project at `root`.
///
/// A mention that names nothing is not reported unless it looked like a path
/// (it contains a `/` or a `.`), so `@alice` in running text stays silent.
#[must_use]
pub fn attach(root: &Path, text: &str) -> Attachments {
    let mut out = Attachments::default();
    let mut budget = MAX_TOTAL_BYTES;
    let mut seen: Vec<(String, Option<(usize, usize)>)> = Vec::new();
    for mention in mentions(text) {
        let key = (mention.target.clone(), mention.lines);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if budget == 0 {
            out.skipped.push(Skip {
                target: mention.target,
                reason: format!(
                    "the references in one prompt are limited to {} KiB",
                    MAX_TOTAL_BYTES / 1024
                ),
            });
            continue;
        }
        match resolve(root, &mention, budget.min(MAX_FILE_BYTES)) {
            Ok(reference) => {
                budget = budget.saturating_sub(reference.text.len());
                out.attached.push(reference);
            }
            Err(RefError::Missing) => {
                if mention.target.contains(['/', '.']) {
                    out.skipped.push(Skip {
                        target: mention.target,
                        reason: "no such file in the project".to_owned(),
                    });
                }
            }
            Err(RefError::Refused(reason)) => out.skipped.push(Skip {
                target: mention.target,
                reason,
            }),
        }
    }
    out
}

/// Why one reference could not be resolved.
#[derive(Debug, Eq, PartialEq)]
enum RefError {
    Missing,
    Refused(String),
}

fn refuse<T>(reason: impl Into<String>) -> Result<T, RefError> {
    Err(RefError::Refused(reason.into()))
}

fn resolve(root: &Path, mention: &Mention, byte_budget: usize) -> Result<Reference, RefError> {
    let relative = mention.target.strip_prefix("./").unwrap_or(&mention.target);
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return refuse("only paths inside the project can be attached");
    }
    let joined = root.join(relative_path);
    if fs::symlink_metadata(&joined).is_err() {
        return Err(RefError::Missing);
    }
    let (Ok(real), Ok(real_root)) = (fs::canonicalize(&joined), fs::canonicalize(root)) else {
        return Err(RefError::Missing);
    };
    if !real.starts_with(&real_root) {
        return refuse("that path leads outside the project");
    }
    if real
        .strip_prefix(&real_root)
        .ok()
        .is_some_and(|inside| inside.components().any(|c| c.as_os_str() == ".git"))
    {
        return refuse("the contents of .git are not attached");
    }
    let shown = display_path(&real_root, &real);
    if shown.len() > MAX_PATH_BYTES {
        return refuse("the path is too long to attach");
    }
    let meta = fs::metadata(&real).map_err(|_| RefError::Missing)?;
    if meta.is_dir() {
        return directory_reference(&real, &shown);
    }
    if !meta.is_file() {
        return refuse("not a regular file");
    }
    if looks_like_credentials_file(&shown) {
        return refuse(
            "it looks like a credentials file; ask the agent to read it if you mean to share it",
        );
    }
    file_reference(root, &real, &shown, mention.lines, byte_budget)
}

fn display_path(real_root: &Path, real: &Path) -> String {
    real.strip_prefix(real_root)
        .unwrap_or(real)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn directory_reference(dir: &Path, shown: &str) -> Result<Reference, RefError> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|_| RefError::Missing)?
        .filter_map(Result::ok)
        .filter(|e| e.file_name() != ".git")
        .map(|e| {
            let mut name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                name.push('/');
            }
            name
        })
        .collect();
    names.sort();
    let total = names.len();
    names.truncate(MAX_DIR_ENTRIES);
    let mut body = names.join("\n");
    if total > MAX_DIR_ENTRIES {
        body.push_str(&format!("\n... and {} more", total - MAX_DIR_ENTRIES));
    }
    let label = if shown.is_empty() { "." } else { shown };
    Ok(Reference {
        path: label.to_owned(),
        text: format!(
            "The user attached the directory `{label}/`. Its entries:\n{}",
            fence(&body)
        ),
        summary: format!("{label}/ ({total} entries)"),
    })
}

fn file_reference(
    root: &Path,
    real: &Path,
    shown: &str,
    lines: Option<(usize, usize)>,
    byte_budget: usize,
) -> Result<Reference, RefError> {
    // A line range may sit anywhere in the file, so it reads further than the
    // size it will attach.
    let read_limit = if lines.is_some() {
        MAX_RANGE_SCAN_BYTES
    } else {
        byte_budget
    };
    let mut file = fs::File::open(real).map_err(|_| RefError::Missing)?;
    let mut bytes = Vec::new();
    // One byte past the limit says whether there was more.
    file.by_ref()
        .take(u64::try_from(read_limit.max(1) + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|_| RefError::Missing)?;
    let probe = &bytes[..bytes.len().min(BINARY_PROBE)];
    if probe.contains(&0) {
        return refuse("a binary file cannot be attached");
    }
    let cut = bytes.len() > read_limit;
    bytes.truncate(read_limit);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if cut && let Some(last_newline) = text.rfind('\n') {
        // Back up to a line boundary so a line is never shown half-way.
        text.truncate(last_newline + 1);
    }
    // The whole of what was read is checked, not only the part attached: a
    // private key's header is one line, and a range that starts below it would
    // otherwise carry the body through unflagged.
    match secret_in(root, shown, text.as_bytes()) {
        Ok(None) => {}
        Ok(Some(rules)) => {
            return refuse(format!("it looks like it contains a secret ({rules})"));
        }
        Err(_) => return refuse("it could not be checked for secrets"),
    }
    let total_lines = text.lines().count();
    let (mut body, range_note) = match lines {
        Some((first, last)) => {
            let picked: Vec<&str> = text
                .lines()
                .skip(first - 1)
                .take(last - first + 1)
                .collect();
            if picked.is_empty() {
                return refuse(if cut {
                    format!(
                        "the range is beyond the first {} MiB read",
                        MAX_RANGE_SCAN_BYTES / (1024 * 1024)
                    )
                } else {
                    format!("the file has only {total_lines} lines")
                });
            }
            let end = first + picked.len() - 1;
            (picked.join("\n"), format!(" (lines {first}-{end})"))
        }
        None => (text.trim_end_matches('\n').to_owned(), String::new()),
    };
    // What is attached is bounded whether it is the file or a range of it.
    let mut shortened = cut && lines.is_none();
    if body.len() > byte_budget {
        let mut end = byte_budget;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        if let Some(newline) = body.rfind('\n') {
            body.truncate(newline);
        }
        shortened = true;
    }
    let truncation = if shortened {
        format!("\n[cut after {} KiB; there is more]", byte_budget / 1024)
    } else {
        String::new()
    };
    let printable = printable_path(shown);
    Ok(Reference {
        path: shown.to_owned(),
        summary: format!("{printable}{range_note}, {} bytes", body.len()),
        text: format!(
            "The user attached `{printable}`{range_note}:\n{}{truncation}",
            fence(&body)
        ),
    })
}

/// `path` fit to appear in a heading line: no control characters and no
/// backticks, which could close the quoting around it.
fn printable_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_control() || c == '`' { '?' } else { c })
        .collect()
}

/// Whether a file's name says it holds credentials. Content scanning catches
/// what its rules know; the name catches the rest (a password too short or
/// plain to look like a key).
fn looks_like_credentials_file(relative: &str) -> bool {
    let name = relative
        .rsplit('/')
        .next()
        .unwrap_or(relative)
        .to_ascii_lowercase();
    let template = [".example", ".sample", ".template", ".dist", ".defaults"]
        .iter()
        .any(|suffix| name.ends_with(suffix));
    if template {
        return false;
    }
    name == ".env"
        || name.starts_with(".env.")
        || name == ".netrc"
        || name == ".npmrc"
        || name == ".pypirc"
        || name == "credentials"
        || name == "secrets.json"
        || name == "secrets.yml"
        || name == "secrets.yaml"
        || name.starts_with("id_rsa")
        || name.starts_with("id_dsa")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_ed25519")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks", ".kdbx"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// A rule name if `content` looks like it holds a secret; `Err` when it could
/// not be checked.
fn secret_in(root: &Path, path: &str, content: &[u8]) -> Result<Option<String>, String> {
    crate::exec_tools::secret_rules_in(root, path, content)
}

/// `body` in a code fence long enough that nothing inside can close it.
fn fence(body: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let ticks = "`".repeat(longest.max(2) + 1);
    format!("{ticks}\n{body}\n{ticks}")
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

/// The reference being typed at the end of `text`, if any: its byte offset
/// (of the `@`) and the characters after it, with any opening quote removed.
///
/// A reference is the last whitespace-delimited word when it begins with `@`,
/// so `@packages/@scope/x` is one reference and an `@` inside a word is not the
/// start of one. A quoted reference (`@"my notes/a`) may hold spaces, and runs
/// to the end of the text while its quote is still open.
#[must_use]
pub fn trailing_token(text: &str) -> Option<(usize, &str)> {
    if let Some(at) = text.rfind("@\"") {
        let after = &text[at + 2..];
        if !after.contains(['"', '\n']) && opens_mention(text[..at].chars().next_back()) {
            return Some((at, after));
        }
    }
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let word = &text[start..];
    let trimmed = word.trim_start_matches(['(', '[', '{', '"', '\'', '`']);
    let at = start + (word.len() - trimmed.len());
    trimmed.strip_prefix('@').map(|rest| (at, rest))
}

/// What Tab did to a reference being typed.
#[derive(Debug, Eq, PartialEq)]
pub struct Completion {
    /// The text the token should become, `@` included.
    pub replacement: String,
    /// Other paths that matched, best first (empty when one match was taken).
    pub candidates: Vec<String>,
}

/// Complete the reference `partial` (the characters after the `@`).
///
/// A name that begins with `partial` wins over a looser match; when nothing
/// begins with it, files are ranked by how well their path fits it as a
/// subsequence, with a bonus where letters start a path segment.
#[must_use]
pub fn complete(root: &Path, partial: &str) -> Option<Completion> {
    let files = project_files(root);
    let ranked = rank(&files, partial);
    let first = ranked.first()?;
    if ranked.len() == 1 {
        return Some(Completion {
            replacement: format!("@{}", finish(first)),
            candidates: Vec::new(),
        });
    }
    let shared = common_prefix(&ranked);
    let typed_length = partial.chars().count();
    let replacement = if shared.chars().count() > typed_length
        && shared.to_lowercase().starts_with(&partial.to_lowercase())
    {
        format!("@{}", open_quote_if_needed(&shared))
    } else {
        format!("@{partial}")
    };
    Some(Completion {
        replacement,
        candidates: ranked.into_iter().take(MAX_CANDIDATES).collect(),
    })
}

/// A path ready to follow `@` when it is complete: a file ends the reference
/// (closing any quote, then a space); a directory leaves the cursor inside it,
/// quote still open if the name needs one.
fn finish(path: &str) -> String {
    let spaced = path.contains(char::is_whitespace);
    match (path.ends_with('/'), spaced) {
        (true, true) => format!("\"{path}"),
        (true, false) => path.to_owned(),
        (false, true) => format!("\"{path}\" "),
        (false, false) => format!("{path} "),
    }
}

/// A partial path, with an opening quote if its name needs one.
fn open_quote_if_needed(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("\"{path}")
    } else {
        path.to_owned()
    }
}

fn common_prefix(paths: &[String]) -> String {
    let Some(first) = paths.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for other in &paths[1..] {
        let common = prefix
            .iter()
            .zip(other.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        prefix.truncate(common);
    }
    prefix.into_iter().collect()
}

/// The project's files and directories, as relative `/` paths.
///
/// Uses the project's own notion of "not ignored" when it is a git checkout, so
/// build output and dependencies do not drown the matches.
#[must_use]
pub fn project_files(root: &Path) -> Vec<String> {
    if let Some(listed) = git_files(root) {
        return listed;
    }
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out
}

/// How long `git ls-files` may take before completion falls back to walking.
const LISTING_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a listing is reused. Tab is pressed in bursts.
const LISTING_TTL: Duration = Duration::from_secs(2);

type ListingCache = Mutex<Option<(PathBuf, Instant, Vec<String>)>>;

static LISTING: ListingCache = Mutex::new(None);

fn git_files(root: &Path) -> Option<Vec<String>> {
    if let Some((cached_root, at, files)) = LISTING.lock().ok()?.as_ref()
        && cached_root == root
        && at.elapsed() < LISTING_TTL
    {
        return Some(files.clone());
    }
    let listed = list_with_git(root)?;
    if let Ok(mut slot) = LISTING.lock() {
        *slot = Some((root.to_path_buf(), Instant::now(), listed.clone()));
    }
    Some(listed)
}

fn list_with_git(root: &Path) -> Option<Vec<String>> {
    // `core.fsmonitor` is a program named by the repository's own config; a
    // listing must not run it. Bounded in time: a hung git must not hang Tab.
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + LISTING_TIMEOUT;
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let stdout = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    let output = std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    };
    let mut files: Vec<String> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .take(MAX_INDEXED_FILES)
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    // Directories as well, so `@src/` completes.
    let mut dirs = std::collections::BTreeSet::new();
    for file in &files {
        let mut path = PathBuf::new();
        let parts: Vec<&str> = file.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            path.push(part);
            dirs.insert(format!("{}/", path.to_string_lossy().replace('\\', "/")));
        }
    }
    files.extend(dirs);
    files.sort();
    files.dedup();
    Some(files)
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 8 || out.len() >= MAX_INDEXED_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if kind.is_dir() {
            if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            out.push(format!("{relative}/"));
            walk(root, &entry.path(), depth + 1, out);
        } else if kind.is_file() && !name.starts_with('.') {
            out.push(relative);
        }
    }
}

/// `candidates` ordered by fit to `partial`; empty `partial` lists the top level.
fn rank(candidates: &[String], partial: &str) -> Vec<String> {
    let needle = partial.to_lowercase();
    if needle.is_empty() {
        return candidates
            .iter()
            .filter(|c| c.trim_end_matches('/').matches('/').count() == 0)
            .cloned()
            .collect();
    }
    let mut prefixed: Vec<&String> = candidates
        .iter()
        .filter(|c| c.to_lowercase().starts_with(&needle))
        .collect();
    if !prefixed.is_empty() {
        // Entries directly in the directory typed so far come before deeper ones.
        prefixed.sort_by_key(|c| (c.matches('/').count(), c.len()));
        return prefixed.into_iter().cloned().collect();
    }
    let mut scored: Vec<(i64, &String)> = candidates
        .iter()
        .filter_map(|c| fuzzy_score(c, &needle).map(|s| (s, c)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(b.1))
    });
    scored.into_iter().map(|(_, c)| c.clone()).collect()
}

/// Subsequence match of `needle` (lowercase) in `candidate`, or `None`.
/// Higher is better: letters that follow each other, or start a path segment or
/// a word, score more; a longer path scores a little less.
fn fuzzy_score(candidate: &str, needle: &str) -> Option<i64> {
    let hay: Vec<char> = candidate.chars().collect();
    let mut score: i64 = 0;
    let mut at = 0;
    let mut previous_matched = false;
    for wanted in needle.chars() {
        let mut matched = false;
        while at < hay.len() {
            let c = hay[at];
            at += 1;
            if c.to_lowercase().next() == Some(wanted) {
                score += 10;
                if previous_matched {
                    score += 15;
                }
                let boundary = at == 1
                    || matches!(hay[at - 2], '/' | '_' | '-' | '.' | ' ')
                    || (hay[at - 2].is_lowercase() && c.is_uppercase());
                if boundary {
                    score += 20;
                }
                matched = true;
                break;
            }
            previous_matched = false;
        }
        if !matched {
            return None;
        }
        previous_matched = true;
    }
    // A match inside the file name beats one spread over directories.
    let name_start = candidate
        .trim_end_matches('/')
        .rfind('/')
        .map_or(0, |i| i + 1);
    let name = candidate[name_start..].to_lowercase();
    if name.contains(needle) {
        score += 40;
    }
    Some(score - i64::try_from(hay.len()).unwrap_or(0) / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rapid-refs-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("dir");
            Self(path)
        }

        fn put(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            fs::write(path, text).expect("write");
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn targets(text: &str) -> Vec<(String, Option<(usize, usize)>)> {
        mentions(text)
            .into_iter()
            .map(|m| (m.target, m.lines))
            .collect()
    }

    #[test]
    fn mentions_are_found_at_word_starts_and_trailing_punctuation_is_dropped() {
        assert_eq!(
            targets("look at @src/lib.rs, then (@docs/a.md) and @x."),
            [
                ("src/lib.rs".to_owned(), None),
                ("docs/a.md".to_owned(), None),
                ("x".to_owned(), None)
            ]
        );
    }

    #[test]
    fn an_email_address_or_a_mid_word_at_sign_is_not_a_mention() {
        assert!(targets("mail me at a@b.com or c@d").is_empty());
        assert!(targets("a lone @ and @@ and @ x").is_empty());
    }

    #[test]
    fn line_ranges_are_read_in_both_spellings() {
        assert_eq!(
            targets("@a.rs:10-20"),
            [("a.rs".to_owned(), Some((10, 20)))]
        );
        assert_eq!(targets("@a.rs#L5-L9"), [("a.rs".to_owned(), Some((5, 9)))]);
        assert_eq!(targets("@a.rs:7"), [("a.rs".to_owned(), Some((7, 7)))]);
        // An impossible range is part of the name, not a range.
        assert_eq!(targets("@a.rs:9-3"), [("a.rs:9-3".to_owned(), None)]);
        assert_eq!(targets("@a.rs:0"), [("a.rs:0".to_owned(), None)]);
    }

    #[test]
    fn a_quoted_target_may_contain_spaces_and_must_close() {
        assert_eq!(
            targets(r#"see @"my notes/a b.txt" now"#),
            [("my notes/a b.txt".to_owned(), None)]
        );
        assert!(targets(r#"see @"unclosed path"#).is_empty());
    }

    #[test]
    fn at_most_sixteen_mentions_are_taken() {
        let text = (0..40)
            .map(|i| format!("@f{i}.txt"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(mentions(&text).len(), MAX_MENTIONS);
    }

    #[test]
    fn a_file_is_attached_with_its_contents_in_a_fence() {
        let d = Dir::new("attach");
        d.put("src/lib.rs", "fn main() {}\n");
        let out = attach(&d.0, "explain @src/lib.rs please");
        assert_eq!(out.attached.len(), 1, "{out:?}");
        assert_eq!(out.attached[0].path, "src/lib.rs");
        assert!(out.attached[0].text.contains("fn main() {}"));
        assert!(out.attached[0].text.contains("```"));
        assert!(out.skipped.is_empty());
    }

    #[test]
    fn a_line_range_attaches_only_those_lines() {
        let d = Dir::new("range");
        d.put("a.txt", "one\ntwo\nthree\nfour\n");
        let out = attach(&d.0, "@a.txt:2-3");
        let text = &out.attached[0].text;
        assert!(
            text.contains("two\nthree") && !text.contains("one") && !text.contains("four"),
            "{text}"
        );
        assert!(text.contains("lines 2-3"));
        let past = attach(&d.0, "@a.txt:50-60");
        assert!(past.attached.is_empty());
        assert!(past.skipped[0].reason.contains("only 4 lines"), "{past:?}");
    }

    #[test]
    fn a_directory_is_listed_not_read() {
        let d = Dir::new("dir");
        d.put("src/a.rs", "");
        d.put("src/sub/b.rs", "");
        let out = attach(&d.0, "@src/");
        assert!(out.attached[0].text.contains("a.rs"));
        assert!(out.attached[0].text.contains("sub/"));
    }

    #[test]
    fn paths_outside_the_project_are_refused() {
        let d = Dir::new("escape");
        d.put("inside.txt", "ok");
        let outside = Dir::new("escape-outside");
        outside.put("secret.txt", "private");
        let absolute = format!("@{}", outside.0.join("secret.txt").display());
        for text in ["@../x.txt".to_owned(), absolute] {
            let out = attach(&d.0, &text);
            assert!(out.attached.is_empty(), "{text}: {out:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leads_outside_the_project_is_refused() {
        let d = Dir::new("link");
        let outside = Dir::new("link-outside");
        outside.put("secret.txt", "private");
        std::os::unix::fs::symlink(outside.0.join("secret.txt"), d.0.join("leak.txt"))
            .expect("link");
        let out = attach(&d.0, "@leak.txt");
        assert!(out.attached.is_empty());
        assert!(
            out.skipped[0].reason.contains("outside the project"),
            "{out:?}"
        );
    }

    #[test]
    fn the_contents_of_dot_git_are_not_attached() {
        let d = Dir::new("git");
        d.put(".git/config", "[core]");
        let out = attach(&d.0, "@.git/config");
        assert!(out.attached.is_empty());
        assert!(out.skipped[0].reason.contains(".git"));
    }

    #[test]
    fn binary_files_are_refused() {
        let d = Dir::new("bin");
        fs::write(d.0.join("a.bin"), [1u8, 2, 0, 3]).expect("write");
        let out = attach(&d.0, "@a.bin");
        assert!(out.attached.is_empty());
        assert!(out.skipped[0].reason.contains("binary"));
    }

    #[test]
    fn a_large_file_is_cut_at_a_line_boundary_and_says_so() {
        let d = Dir::new("big");
        let line = "0123456789abcdef\n";
        d.put("big.txt", &line.repeat(MAX_FILE_BYTES / line.len() + 500));
        let out = attach(&d.0, "@big.txt");
        let text = &out.attached[0].text;
        assert!(
            text.contains("[cut after 64 KiB"),
            "{}",
            &text[text.len().saturating_sub(120)..]
        );
        assert!(text.len() <= MAX_FILE_BYTES + 400);
        let body = text.split("```").nth(1).expect("fenced body");
        assert!(
            body.trim_end().ends_with("0123456789abcdef"),
            "no half line"
        );
    }

    #[test]
    fn a_file_that_looks_like_it_holds_a_secret_is_refused() {
        let d = Dir::new("secret");
        d.put(
            "creds.txt",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n",
        );
        let out = attach(&d.0, "@creds.txt");
        assert!(out.attached.is_empty(), "{out:?}");
        assert!(out.skipped[0].reason.contains("secret"), "{out:?}");
    }

    #[test]
    fn a_missing_plain_word_is_silent_but_a_missing_path_is_reported() {
        let d = Dir::new("missing");
        let out = attach(&d.0, "ping @alice about @src/nope.rs");
        assert!(out.attached.is_empty());
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].target, "src/nope.rs");
    }

    #[test]
    fn the_same_reference_twice_is_attached_once() {
        let d = Dir::new("dupe");
        d.put("a.txt", "x");
        let out = attach(&d.0, "@a.txt and again @a.txt");
        assert_eq!(out.attached.len(), 1);
    }

    #[test]
    fn a_fence_is_longer_than_any_run_of_backticks_inside() {
        let fenced = fence("a ```` b");
        assert!(fenced.starts_with("`````\n"), "{fenced}");
    }

    #[test]
    fn the_trailing_token_is_the_last_unfinished_reference() {
        assert_eq!(trailing_token("see @src/li"), Some((4, "src/li")));
        assert_eq!(trailing_token("see @src/lib.rs now"), None);
        assert_eq!(trailing_token("mail a@b"), None);
        assert_eq!(trailing_token("no reference"), None);
        assert_eq!(trailing_token("@"), Some((0, "")));
    }

    #[test]
    fn completion_takes_a_unique_prefix_and_extends_a_shared_one() {
        let d = Dir::new("complete");
        d.put("src/main.rs", "");
        d.put("src/manifest.rs", "");
        d.put("docs/guide.md", "");
        let unique = complete(&d.0, "docs/g").expect("unique");
        assert_eq!(unique.replacement, "@docs/guide.md ");
        assert!(unique.candidates.is_empty());

        let shared = complete(&d.0, "src/ma").expect("shared");
        assert_eq!(shared.replacement, "@src/ma");
        assert_eq!(shared.candidates, ["src/main.rs", "src/manifest.rs"]);
        let longer = complete(&d.0, "src/man").expect("man");
        assert_eq!(longer.replacement, "@src/manifest.rs ");
    }

    #[test]
    fn completion_falls_back_to_a_fuzzy_match_across_the_tree() {
        let d = Dir::new("fuzzy");
        d.put("crates/tui/src/composer.rs", "");
        d.put("crates/kernel/src/client.rs", "");
        d.put("README.md", "");
        let hit = complete(&d.0, "comp").expect("fuzzy");
        assert_eq!(hit.replacement, "@crates/tui/src/composer.rs ");
        // Letters spread over segments still find the file, best match first.
        let spread = complete(&d.0, "kcl").expect("spread");
        assert_eq!(spread.replacement, "@crates/kernel/src/client.rs ");
        assert!(complete(&d.0, "zzzzqq").is_none());
    }

    #[test]
    fn completing_a_directory_leaves_the_cursor_inside_it() {
        let d = Dir::new("dircomplete");
        d.put("docs/guide.md", "");
        let hit = complete(&d.0, "do").expect("dir");
        assert_eq!(hit.replacement, "@docs/");
    }

    #[test]
    fn a_path_with_spaces_is_quoted_when_completed() {
        let d = Dir::new("spaces");
        d.put("my notes/a.txt", "");
        let hit = complete(&d.0, "my").expect("quoted");
        assert_eq!(
            hit.replacement, "@\"my notes/",
            "a directory leaves the quote open"
        );
    }

    #[test]
    fn ignored_files_do_not_appear_in_completion_inside_a_git_checkout() {
        let d = Dir::new("ignored");
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(&d.0)
                .status()
                .is_ok_and(|s| s.success())
        );
        d.put(".gitignore", "build/\n");
        d.put("build/out.txt", "");
        d.put("src/out.rs", "");
        let files = project_files(&d.0);
        assert!(files.contains(&"src/out.rs".to_owned()), "{files:?}");
        assert!(!files.iter().any(|f| f.starts_with("build/")), "{files:?}");
    }

    // --- regressions for the review ----------------------------------------

    #[test]
    fn a_range_cannot_carry_a_private_key_body_past_the_secret_check() {
        let d = Dir::new("range-secret");
        d.put(
            "notes.txt",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nabcdef\n-----END RSA PRIVATE KEY-----\n",
        );
        let whole = attach(&d.0, "@notes.txt");
        assert!(whole.attached.is_empty(), "{whole:?}");
        let ranged = attach(&d.0, "@notes.txt:2-3");
        assert!(
            ranged.attached.is_empty(),
            "the header is outside the range: {ranged:?}"
        );
        assert!(ranged.skipped[0].reason.contains("secret"), "{ranged:?}");
    }

    #[test]
    fn a_file_that_cannot_be_checked_for_secrets_is_not_attached() {
        // A name the scanner cannot take as a repository path.
        assert!(secret_in(Path::new("."), "", b"x").is_err());
        let d = Dir::new("unscannable");
        d.put("ok.txt", "plain");
        assert_eq!(attach(&d.0, "@ok.txt").attached.len(), 1);
    }

    #[test]
    fn credentials_files_are_refused_by_name_but_templates_are_not() {
        let d = Dir::new("credfiles");
        for name in [
            ".env",
            ".env.local",
            "config/.npmrc",
            "id_rsa",
            "id_ed25519.pub",
            "server.pem",
            "a/b/tls.key",
            "store.p12",
            "credentials",
        ] {
            d.put(name, "x=1\n");
            let out = attach(&d.0, &format!("@{name}"));
            assert!(out.attached.is_empty(), "{name}: {out:?}");
            assert!(
                out.skipped[0].reason.contains("credentials"),
                "{name}: {out:?}"
            );
        }
        for name in [
            ".env.example",
            "config.sample",
            "app.env.template",
            "notes.txt",
            "keyboard.rs",
        ] {
            d.put(name, "x=1\n");
            assert_eq!(
                attach(&d.0, &format!("@{name}")).attached.len(),
                1,
                "{name}"
            );
        }
    }

    #[test]
    fn a_line_range_works_deep_into_a_file_larger_than_the_attach_limit() {
        let d = Dir::new("deeprange");
        let body: String = (1..=20_000).map(|n| format!("line number {n}\n")).collect();
        assert!(body.len() > MAX_FILE_BYTES * 3);
        d.put("big.log", &body);
        let out = attach(&d.0, "@big.log:15000-15002");
        assert_eq!(out.attached.len(), 1, "{out:?}");
        let text = &out.attached[0].text;
        assert!(
            text.contains("line number 15000\nline number 15001\nline number 15002"),
            "{text}"
        );
        assert!(text.contains("lines 15000-15002"));
        // Past the end, the count is the real one.
        let past = attach(&d.0, "@big.log:30000-30001");
        assert!(
            past.skipped[0].reason.contains("only 20000 lines"),
            "{past:?}"
        );
    }

    #[test]
    fn a_huge_range_is_cut_to_the_attach_limit_and_says_so() {
        let d = Dir::new("hugerange");
        let body: String = (1..=20_000).map(|n| format!("line number {n}\n")).collect();
        d.put("big.log", &body);
        let out = attach(&d.0, "@big.log:1-20000");
        let text = &out.attached[0].text;
        assert!(text.len() <= MAX_FILE_BYTES + 400, "{}", text.len());
        assert!(text.contains("[cut after 64 KiB; there is more]"));
    }

    #[test]
    fn a_scoped_package_directory_completes_even_though_it_contains_an_at_sign() {
        let d = Dir::new("scoped");
        d.put("packages/@scope/pkg/index.js", "");
        assert_eq!(trailing_token("@packages/@sc"), Some((0, "packages/@sc")));
        let hit = complete(&d.0, "packages/@sc").expect("completes");
        assert_eq!(hit.replacement, "@packages/@scope/", "{hit:?}");
        // An `@` inside a word that does not start with one is not a reference.
        assert_eq!(trailing_token("mail a@b"), None);
        assert_eq!(trailing_token("see (@src/li"), Some((5, "src/li")));
    }

    #[test]
    fn a_quoted_name_round_trips_through_completion() {
        let d = Dir::new("roundtrip");
        d.put("my notes/a.txt", "x");
        d.put("my notes/b.txt", "y");
        // A directory leaves the quote open so the person can keep typing.
        let dir = complete(&d.0, "my").expect("dir");
        assert_eq!(dir.replacement, "@\"my notes/");
        let text = format!("look at {}", dir.replacement);
        let (at, partial) = trailing_token(&text).expect("still a reference");
        assert_eq!((at, partial), (8, "my notes/"));
        // Continuing inside the quote completes to a file and closes it.
        let file = complete(&d.0, "my notes/a").expect("file");
        assert_eq!(file.replacement, "@\"my notes/a.txt\" ");
        // The finished text parses back to exactly that file.
        let finished = format!("look at {}now", file.replacement);
        let found = mentions(&finished);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target, "my notes/a.txt");
    }

    #[test]
    fn an_empty_directory_completes_without_a_doubled_slash() {
        let d = Dir::new("emptydir");
        fs::create_dir_all(d.0.join("emptydir")).expect("dir");
        let hit = complete(&d.0, "empt").expect("dir");
        assert_eq!(hit.replacement, "@emptydir/");
    }

    #[test]
    fn non_ascii_letters_match_in_a_fuzzy_search() {
        let d = Dir::new("unicode");
        d.put("docs/Überblick.md", "");
        d.put("docs/other.md", "");
        let hit = complete(&d.0, "üb").expect("fuzzy");
        assert!(hit.replacement.contains("Überblick.md"), "{hit:?}");
    }

    #[test]
    fn a_path_in_a_heading_cannot_close_its_quoting() {
        assert_eq!(printable_path("a`b\nc.txt"), "a?b?c.txt");
    }

    #[cfg(unix)]
    #[test]
    fn listing_files_never_runs_the_repositorys_own_fsmonitor_program() {
        use std::os::unix::fs::PermissionsExt;
        let git = |dir: &Path, args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .is_ok_and(|s| s.success()),
                "git {args:?}"
            );
        };
        // A plain directory name: git runs the monitor through a shell.
        let plain = std::env::temp_dir().join(format!("rapid-fsmon-{}", std::process::id()));
        let _ = fs::remove_dir_all(&plain);
        fs::create_dir_all(&plain).expect("dir");
        let d = Dir(plain);
        git(&d.0, &["init", "-q"]);
        d.put("a.txt", "x");
        // The index exists before the monitor is configured, as in a checkout
        // someone else prepared; git consults the monitor on the next read.
        git(&d.0, &["add", "a.txt"]);
        let marker =
            d.0.join("..")
                .join(format!("fsmon-ran-{}", std::process::id()));
        let hook = d.0.join("hook.sh");
        fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .expect("hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
        git(&d.0, &["config", "core.fsmonitor", &hook.to_string_lossy()]);
        let _ = fs::remove_file(&marker);
        let files = project_files(&d.0);
        let ran = marker.exists();
        let _ = fs::remove_file(&marker);
        assert!(files.contains(&"a.txt".to_owned()), "{files:?}");
        assert!(!ran, "git ran the repository's fsmonitor program");
    }
}
