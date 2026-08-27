//! Byte-content hashing and Tier-1 language/content classification.
//!
//! Classification is content-first: a `.rs` path with binary bytes is metadata-only,
//! never parsed text. Oversized inputs are hashed but never decoded.

use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str;

use protocol::RepoPath;
use sha2::{Digest, Sha256};

use crate::ingest::walk::{
    DEFAULT_BINARY_PROBE_BYTES, DEFAULT_MAX_FILE_BYTES, FileCandidate, IndexEligibility,
    MetadataOnlyReason,
};
use crate::repo_manifest::CancellationToken;

/// SHA-256 digest length in bytes.
pub const CONTENT_HASH_LEN: usize = 32;

/// Lowercase hex length of a SHA-256 digest.
pub const CONTENT_HASH_HEX_LEN: usize = CONTENT_HASH_LEN * 2;

/// Canonical content-address prefix.
pub const CONTENT_HASH_PREFIX: &str = "sha256:";

/// Stream hash/read chunk. Keeps oversized files off the heap as a single buffer.
pub const HASH_CHUNK_BYTES: usize = 64 * 1024;

const CONTENT_HASH_WIRE_LEN: usize = CONTENT_HASH_PREFIX.len() + CONTENT_HASH_HEX_LEN;
const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";
const CANCEL_STRIDE: u32 = 16;
const SHEBANG_MAX_BYTES: usize = 256;

/// Resource bounds for hashing and text eligibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentLimits {
    max_file_bytes: u64,
    binary_probe_bytes: usize,
}

/// SHA-256 of raw file bytes. Wire form is `sha256:` + 64 lowercase hex digits.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ContentHash([u8; CONTENT_HASH_LEN]);

/// PRD Tier-1 languages and structured-text kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SourceLanguage {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Kotlin,
    Swift,
    Ruby,
    Bash,
    Json,
    Yaml,
    Toml,
    Markdown,
}

/// Text versus metadata-only classification after content inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentClass {
    Text { language: Option<SourceLanguage> },
    MetadataOnly(MetadataOnlyReason),
}

/// Hashed and classified candidate. `text` is present only for eligible UTF-8.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedCandidate {
    path: RepoPath,
    content_hash: ContentHash,
    class: ContentClass,
    text: Option<String>,
}

/// Typed load failure. Display never echoes host or repository paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentError {
    Cancelled,
    Io,
    NotRegularFile,
    PathEscapesRoot,
}

impl ContentLimits {
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

    pub fn max_file_bytes_value(&self) -> u64 {
        self.max_file_bytes
    }

    pub fn binary_probe_bytes_value(&self) -> usize {
        self.binary_probe_bytes
    }
}

impl Default for ContentLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            binary_probe_bytes: DEFAULT_BINARY_PROBE_BYTES,
        }
    }
}

impl ContentHash {
    /// Hash `bytes` with SHA-256. Path and extension are not inputs.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut hash = [0u8; CONTENT_HASH_LEN];
        hash.copy_from_slice(&digest);
        Self(hash)
    }

    pub const fn as_digest(&self) -> &[u8; CONTENT_HASH_LEN] {
        &self.0
    }

    fn from_digest(digest: [u8; CONTENT_HASH_LEN]) -> Self {
        Self(digest)
    }

    fn encode_wire(self) -> [u8; CONTENT_HASH_WIRE_LEN] {
        let mut out = [0u8; CONTENT_HASH_WIRE_LEN];
        out[..CONTENT_HASH_PREFIX.len()].copy_from_slice(CONTENT_HASH_PREFIX.as_bytes());
        for (i, byte) in self.0.iter().copied().enumerate() {
            let at = CONTENT_HASH_PREFIX.len() + i * 2;
            out[at] = HEX_TABLE[(byte >> 4) as usize];
            out[at + 1] = HEX_TABLE[(byte & 0x0f) as usize];
        }
        out
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let buf = self.encode_wire();
        f.write_str(str::from_utf8(&buf).expect("content hash wire form is ASCII"))
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ContentHash")
            .field(&self.to_string())
            .finish()
    }
}

impl SourceLanguage {
    pub const ALL: &'static [Self] = &[
        Self::Rust,
        Self::TypeScript,
        Self::JavaScript,
        Self::Python,
        Self::Go,
        Self::Java,
        Self::C,
        Self::Cpp,
        Self::CSharp,
        Self::Kotlin,
        Self::Swift,
        Self::Ruby,
        Self::Bash,
        Self::Json,
        Self::Yaml,
        Self::Toml,
        Self::Markdown,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
            Self::Ruby => "ruby",
            Self::Bash => "bash",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Markdown => "markdown",
        }
    }
}

impl fmt::Display for SourceLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ContentClass {
    pub fn language(self) -> Option<SourceLanguage> {
        match self {
            Self::Text { language } => language,
            Self::MetadataOnly(_) => None,
        }
    }

    pub fn is_text(self) -> bool {
        matches!(self, Self::Text { .. })
    }

    pub fn metadata_reason(self) -> Option<MetadataOnlyReason> {
        match self {
            Self::MetadataOnly(reason) => Some(reason),
            Self::Text { .. } => None,
        }
    }
}

impl LoadedCandidate {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn class(&self) -> ContentClass {
        self.class
    }

    pub fn language(&self) -> Option<SourceLanguage> {
        self.class.language()
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn eligibility(&self) -> IndexEligibility {
        match self.class {
            ContentClass::Text { .. } => IndexEligibility::FullText,
            ContentClass::MetadataOnly(reason) => IndexEligibility::MetadataOnly(reason),
        }
    }
}

impl ContentError {
    fn from_io(_: std::io::Error) -> Self {
        Self::Io
    }
}

/// Hash `bytes` and classify language/content type. Extension never overrides binary bytes.
pub fn load_candidate(
    path: &RepoPath,
    bytes: &[u8],
    limits: &ContentLimits,
    cancel: &CancellationToken,
) -> Result<LoadedCandidate, ContentError> {
    if cancel.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    let content_hash = ContentHash::from_bytes(bytes);
    let (class, text) = classify_bytes(path, bytes, limits);
    Ok(LoadedCandidate {
        path: path.clone(),
        content_hash,
        class,
        text,
    })
}

/// Read a walked candidate, hash all bytes, and refuse to decode binary/oversized files.
pub fn load_file_candidate(
    root: &Path,
    candidate: &FileCandidate,
    limits: &ContentLimits,
    cancel: &CancellationToken,
) -> Result<LoadedCandidate, ContentError> {
    if cancel.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    let abs = resolve_regular_file(root, candidate.path())?;
    let meta = fs::symlink_metadata(&abs).map_err(ContentError::from_io)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(ContentError::NotRegularFile);
    }
    let size = meta.len();
    if size > limits.max_file_bytes {
        let content_hash = hash_file_streaming(&abs, cancel)?;
        return Ok(LoadedCandidate {
            path: candidate.path().clone(),
            content_hash,
            class: ContentClass::MetadataOnly(MetadataOnlyReason::Oversized),
            text: None,
        });
    }
    let bytes = read_bounded_file(&abs, size, limits.max_file_bytes, cancel)?;
    load_candidate(candidate.path(), &bytes, limits, cancel)
}

fn classify_bytes(
    path: &RepoPath,
    bytes: &[u8],
    limits: &ContentLimits,
) -> (ContentClass, Option<String>) {
    if (bytes.len() as u64) > limits.max_file_bytes {
        return (
            ContentClass::MetadataOnly(MetadataOnlyReason::Oversized),
            None,
        );
    }
    if looks_binary(bytes, limits.binary_probe_bytes) {
        return (ContentClass::MetadataOnly(MetadataOnlyReason::Binary), None);
    }
    let Ok(text) = str::from_utf8(bytes) else {
        return (ContentClass::MetadataOnly(MetadataOnlyReason::Binary), None);
    };
    let language = detect_language(path, text);
    (ContentClass::Text { language }, Some(text.to_owned()))
}

fn detect_language(path: &RepoPath, text: &str) -> Option<SourceLanguage> {
    language_from_extension(path).or_else(|| language_from_shebang(text))
}

fn language_from_extension(path: &RepoPath) -> Option<SourceLanguage> {
    let name = file_name(path.as_str());
    let ext = extension_of(name)?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => SourceLanguage::Rust,
        "ts" | "tsx" | "mts" | "cts" => SourceLanguage::TypeScript,
        "js" | "jsx" | "mjs" | "cjs" => SourceLanguage::JavaScript,
        "py" | "pyi" | "pyw" => SourceLanguage::Python,
        "go" => SourceLanguage::Go,
        "java" => SourceLanguage::Java,
        "c" | "h" => SourceLanguage::C,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => SourceLanguage::Cpp,
        "cs" => SourceLanguage::CSharp,
        "kt" | "kts" => SourceLanguage::Kotlin,
        "swift" => SourceLanguage::Swift,
        "rb" => SourceLanguage::Ruby,
        "sh" | "bash" => SourceLanguage::Bash,
        "json" => SourceLanguage::Json,
        "yml" | "yaml" => SourceLanguage::Yaml,
        "toml" => SourceLanguage::Toml,
        "md" | "markdown" => SourceLanguage::Markdown,
        _ => return None,
    })
}

fn language_from_shebang(text: &str) -> Option<SourceLanguage> {
    let first = text.lines().next()?;
    let first = first.strip_suffix('\r').unwrap_or(first);
    if first.len() > SHEBANG_MAX_BYTES {
        return None;
    }
    let rest = first.strip_prefix("#!")?.trim();
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.split_whitespace();
    let first_tok = parts.next()?;
    let cmd = if interpreter_basename(first_tok) == "env" {
        parts.next()?
    } else {
        first_tok
    };
    let base = interpreter_basename(cmd);
    let stem = interpreter_stem(base);
    match stem {
        "python" => Some(SourceLanguage::Python),
        "node" | "nodejs" => Some(SourceLanguage::JavaScript),
        "ruby" => Some(SourceLanguage::Ruby),
        "bash" | "sh" => Some(SourceLanguage::Bash),
        _ => None,
    }
}

fn interpreter_basename(cmd: &str) -> &str {
    cmd.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cmd)
}

fn interpreter_stem(base: &str) -> &'static str {
    let lower = base.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or(lower.as_str());
    if stem.starts_with("python") {
        "python"
    } else if stem.starts_with("nodejs") {
        "nodejs"
    } else if stem.starts_with("node") {
        "node"
    } else if stem.starts_with("ruby") {
        "ruby"
    } else if stem == "bash" {
        "bash"
    } else if stem == "sh" {
        "sh"
    } else {
        ""
    }
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn extension_of(name: &str) -> Option<&str> {
    if name.starts_with('.') && !name[1..].contains('.') {
        return None;
    }
    let ext = name.rsplit_once('.')?.1;
    if ext.is_empty() { None } else { Some(ext) }
}

fn looks_binary(bytes: &[u8], probe_bytes: usize) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let n = if probe_bytes == 0 {
        bytes.len()
    } else {
        bytes.len().min(probe_bytes)
    };
    let probe = &bytes[..n];
    if probe.contains(&0) {
        return true;
    }
    if has_binary_magic(probe) {
        return true;
    }
    false
}

fn has_binary_magic(probe: &[u8]) -> bool {
    if probe.len() >= 4 && probe.starts_with(b"\x7fELF") {
        return true;
    }
    if probe.len() >= 4 && probe.starts_with(b"\0asm") {
        return true;
    }
    if probe.len() >= 8 && probe.starts_with(b"\x89PNG\r\n\x1a\n") {
        return true;
    }
    if probe.len() >= 3 && probe.starts_with(b"\xff\xd8\xff") {
        return true;
    }
    if probe.len() >= 6 && (probe.starts_with(b"GIF87a") || probe.starts_with(b"GIF89a")) {
        return true;
    }
    if probe.len() >= 4 && probe.starts_with(b"%PDF") {
        return true;
    }
    if probe.len() >= 2 && probe.starts_with(b"\x1f\x8b") {
        return true;
    }
    if probe.len() >= 4
        && (probe.starts_with(b"PK\x03\x04")
            || probe.starts_with(b"PK\x05\x06")
            || probe.starts_with(b"PK\x07\x08"))
    {
        return true;
    }
    if probe.len() >= 4
        && (probe.starts_with(b"\xca\xfe\xba\xbe")
            || probe.starts_with(b"\xfe\xed\xfa\xce")
            || probe.starts_with(b"\xce\xfa\xed\xfe")
            || probe.starts_with(b"\xfe\xed\xfa\xcf")
            || probe.starts_with(b"\xcf\xfa\xed\xfe"))
    {
        return true;
    }
    if probe.len() >= 2 && probe.starts_with(b"MZ") {
        return true;
    }
    false
}

fn resolve_regular_file(root: &Path, rel: &RepoPath) -> Result<PathBuf, ContentError> {
    let root_meta = fs::symlink_metadata(root).map_err(ContentError::from_io)?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(ContentError::PathEscapesRoot);
    }
    let root_canon = fs::canonicalize(root).map_err(ContentError::from_io)?;
    let abs = abs_in(&root_canon, rel.as_str());
    let meta = fs::symlink_metadata(&abs).map_err(ContentError::from_io)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(ContentError::NotRegularFile);
    }
    let parent = abs.parent().ok_or(ContentError::PathEscapesRoot)?;
    let parent_canon = fs::canonicalize(parent).map_err(ContentError::from_io)?;
    if !path_within(&parent_canon, &root_canon) {
        return Err(ContentError::PathEscapesRoot);
    }
    Ok(abs)
}

fn read_bounded_file(
    path: &Path,
    expected: u64,
    max_file_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, ContentError> {
    if expected > max_file_bytes {
        return Err(ContentError::Io);
    }
    let mut file = File::open(path).map_err(ContentError::from_io)?;
    let cap = usize::try_from(expected).map_err(|_| ContentError::Io)?;
    let mut buf = Vec::new();
    if buf.try_reserve_exact(cap).is_err() {
        return Err(ContentError::Io);
    }
    let mut chunk = [0u8; HASH_CHUNK_BYTES];
    let mut steps = 0u32;
    loop {
        steps = steps.wrapping_add(1);
        if (steps == 1 || steps.is_multiple_of(CANCEL_STRIDE))
            && cancel.is_cancelled() {
                return Err(ContentError::Cancelled);
            }
        let n = file.read(&mut chunk).map_err(ContentError::from_io)?;
        if n == 0 {
            break;
        }
        if (buf.len() as u64).saturating_add(n as u64) > max_file_bytes {
            return Err(ContentError::Io);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

fn hash_file_streaming(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<ContentHash, ContentError> {
    let mut file = File::open(path).map_err(ContentError::from_io)?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; HASH_CHUNK_BYTES];
    let mut steps = 0u32;
    loop {
        steps = steps.wrapping_add(1);
        if (steps == 1 || steps.is_multiple_of(CANCEL_STRIDE))
            && cancel.is_cancelled() {
                return Err(ContentError::Cancelled);
            }
        let n = file.read(&mut chunk).map_err(ContentError::from_io)?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    let digest = hasher.finalize();
    let mut hash = [0u8; CONTENT_HASH_LEN];
    hash.copy_from_slice(&digest);
    Ok(ContentHash::from_digest(hash))
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

impl fmt::Display for ContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "context content load cancelled",
            Self::Io => "context content load failed due to an I/O error",
            Self::NotRegularFile => "context content path is not a regular file",
            Self::PathEscapesRoot => "context content path is outside the repository root",
        })
    }
}

impl Error for ContentError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::walk::{WalkLimits, walk_repo};
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
                "rapidlm-context-content-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("temp workspace");
            Self { path }
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

    fn parse_manifest(ws: &TempWorkspace) -> WorkspaceManifest {
        ws.write_file("core/.keep", b"");
        let src =
            "schema = 1\n[[repos]]\nalias = \"core\"\nroot = \"core\"\nmode = \"read_write\"\n";
        WorkspaceManifest::parse(src, &ws.path, &CancellationToken::new()).expect("manifest")
    }

    fn path(rel: &str) -> RepoPath {
        RepoPath::parse(rel).expect("repo path")
    }

    fn load(rel: &str, bytes: &[u8]) -> LoadedCandidate {
        load_candidate(
            &path(rel),
            bytes,
            &ContentLimits::default(),
            &CancellationToken::new(),
        )
        .expect("load")
    }

    #[test]
    fn hash_is_byte_content_based_and_path_independent() {
        let a = load("src/a.rs", b"fn main() {}");
        let b = load("other/name.py", b"fn main() {}");
        let c = load("src/a.rs", b"fn main() { }");
        assert_eq!(a.content_hash(), b.content_hash());
        assert_ne!(a.content_hash(), c.content_hash());
        assert_eq!(ContentHash::from_bytes(b"fn main() {}"), a.content_hash());
        assert_eq!(
            a.content_hash().to_string(),
            format!("sha256:{}", {
                let d = Sha256::digest(b"fn main() {}");
                d.iter().map(|b| format!("{b:02x}")).collect::<String>()
            })
        );
        assert!(a.content_hash().to_string().starts_with("sha256:"));
        assert_eq!(a.content_hash().to_string().len(), CONTENT_HASH_WIRE_LEN);
    }

    #[test]
    fn known_sha256_vector_matches_empty_and_abc() {
        assert_eq!(
            ContentHash::from_bytes(b"").to_string(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            ContentHash::from_bytes(b"abc").to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn tier1_extensions_map_to_languages() {
        let cases = [
            ("lib.rs", SourceLanguage::Rust),
            ("app.ts", SourceLanguage::TypeScript),
            ("view.tsx", SourceLanguage::TypeScript),
            ("index.js", SourceLanguage::JavaScript),
            ("mod.mjs", SourceLanguage::JavaScript),
            ("main.py", SourceLanguage::Python),
            ("types.pyi", SourceLanguage::Python),
            ("main.go", SourceLanguage::Go),
            ("Main.java", SourceLanguage::Java),
            ("core.c", SourceLanguage::C),
            ("api.h", SourceLanguage::C),
            ("net.cpp", SourceLanguage::Cpp),
            ("util.hpp", SourceLanguage::Cpp),
            ("Program.cs", SourceLanguage::CSharp),
            ("App.kt", SourceLanguage::Kotlin),
            ("build.kts", SourceLanguage::Kotlin),
            ("View.swift", SourceLanguage::Swift),
            ("gem.rb", SourceLanguage::Ruby),
            ("setup.sh", SourceLanguage::Bash),
            ("data.json", SourceLanguage::Json),
            ("cfg.yaml", SourceLanguage::Yaml),
            ("cfg.yml", SourceLanguage::Yaml),
            ("Cargo.toml", SourceLanguage::Toml),
            ("README.md", SourceLanguage::Markdown),
        ];
        for (rel, want) in cases {
            let loaded = load(rel, b"x = 1\n");
            assert_eq!(loaded.language(), Some(want), "{rel}");
            assert_eq!(
                loaded.class(),
                ContentClass::Text {
                    language: Some(want)
                }
            );
            assert_eq!(loaded.text(), Some("x = 1\n"));
            assert_eq!(loaded.eligibility(), IndexEligibility::FullText);
        }
    }

    #[test]
    fn shebang_detects_script_without_extension() {
        let py = load("scripts/run", b"#!/usr/bin/env python3\nprint(1)\n");
        assert_eq!(py.language(), Some(SourceLanguage::Python));
        let sh = load("scripts/boot", b"#!/bin/bash\necho hi\n");
        assert_eq!(sh.language(), Some(SourceLanguage::Bash));
        let rb = load("scripts/tool", b"#!/usr/bin/env ruby\nputs 1\n");
        assert_eq!(rb.language(), Some(SourceLanguage::Ruby));
        let js = load("scripts/cli", b"#!/usr/bin/env node\nconsole.log(1)\n");
        assert_eq!(js.language(), Some(SourceLanguage::JavaScript));
    }

    #[test]
    fn binary_payload_is_metadata_only_even_with_source_extension() {
        let elf = {
            let mut bytes = b"\x7fELFnot-rust".to_vec();
            bytes.extend_from_slice(&[0x00, 0x01, 0x02]);
            bytes
        };
        let loaded = load("src/lib.rs", &elf);
        assert_eq!(
            loaded.class(),
            ContentClass::MetadataOnly(MetadataOnlyReason::Binary)
        );
        assert_eq!(loaded.language(), None);
        assert_eq!(loaded.text(), None);
        assert_eq!(
            loaded.eligibility(),
            IndexEligibility::MetadataOnly(MetadataOnlyReason::Binary)
        );
        assert_eq!(loaded.content_hash(), ContentHash::from_bytes(&elf));
    }

    #[test]
    fn nul_bytes_are_binary_regardless_of_extension() {
        let loaded = load("notes.md", b"hello\0world");
        assert_eq!(
            loaded.class(),
            ContentClass::MetadataOnly(MetadataOnlyReason::Binary)
        );
        assert_eq!(loaded.text(), None);
    }

    #[test]
    fn png_magic_is_binary_despite_python_extension() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[1, 2, 3, 4]);
        let loaded = load("plot.py", &png);
        assert_eq!(
            loaded.class(),
            ContentClass::MetadataOnly(MetadataOnlyReason::Binary)
        );
        assert_eq!(loaded.language(), None);
        assert_eq!(loaded.text(), None);
    }

    #[test]
    fn invalid_utf8_is_metadata_only_not_parsed_text() {
        let loaded = load("plain.txt", &[0xff, 0xfe, 0xfd]);
        assert_eq!(
            loaded.class(),
            ContentClass::MetadataOnly(MetadataOnlyReason::Binary)
        );
        assert_eq!(loaded.text(), None);
    }

    #[test]
    fn oversized_bytes_are_hashed_but_not_parsed() {
        let bytes = vec![b'a'; 64];
        let loaded = load_candidate(
            &path("huge.rs"),
            &bytes,
            &ContentLimits::new().max_file_bytes(16),
            &CancellationToken::new(),
        )
        .expect("load");
        assert_eq!(
            loaded.class(),
            ContentClass::MetadataOnly(MetadataOnlyReason::Oversized)
        );
        assert_eq!(loaded.language(), None);
        assert_eq!(loaded.text(), None);
        assert_eq!(loaded.content_hash(), ContentHash::from_bytes(&bytes));
        assert_eq!(
            loaded.eligibility(),
            IndexEligibility::MetadataOnly(MetadataOnlyReason::Oversized)
        );
    }

    #[test]
    fn unknown_text_extension_is_text_without_language() {
        let loaded = load("notes.txt", b"hello\n");
        assert_eq!(loaded.class(), ContentClass::Text { language: None });
        assert_eq!(loaded.text(), Some("hello\n"));
    }

    #[test]
    fn cancelled_load_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = load_candidate(
            &path("a.rs"),
            b"fn x() {}",
            &ContentLimits::default(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, ContentError::Cancelled);
        assert_eq!(err.to_string(), "context content load cancelled");
        assert!(!err.to_string().contains("a.rs"));
    }

    #[test]
    fn load_file_candidate_hashes_and_classifies_walked_files() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"pub fn x() {}");
        ws.write_file("core/blob.bin", &[0x00, 0x01, 0x02, 0x00]);
        ws.write_file("core/huge.rs", &[b'a'; 64]);
        let manifest = parse_manifest(&ws);
        let repo = manifest.repo_by_alias("core").expect("repo");
        let walk_limits = WalkLimits::new().max_file_bytes(16).binary_probe_bytes(8);
        let content_limits = ContentLimits::new()
            .max_file_bytes(16)
            .binary_probe_bytes(8);
        let cancel = CancellationToken::new();
        let mut seen_rs = false;
        let mut seen_bin = false;
        let mut seen_huge = false;
        for item in walk_repo(repo, &walk_limits, &cancel) {
            let candidate = item.expect("candidate");
            let loaded =
                load_file_candidate(repo.root().as_path(), &candidate, &content_limits, &cancel)
                    .expect("load file");
            match candidate.path().as_str() {
                "src/lib.rs" => {
                    seen_rs = true;
                    assert_eq!(loaded.language(), Some(SourceLanguage::Rust));
                    assert_eq!(loaded.text(), Some("pub fn x() {}"));
                    assert_eq!(
                        loaded.content_hash(),
                        ContentHash::from_bytes(b"pub fn x() {}")
                    );
                }
                "blob.bin" => {
                    seen_bin = true;
                    assert_eq!(
                        loaded.class(),
                        ContentClass::MetadataOnly(MetadataOnlyReason::Binary)
                    );
                    assert_eq!(loaded.text(), None);
                }
                "huge.rs" => {
                    seen_huge = true;
                    assert_eq!(
                        loaded.class(),
                        ContentClass::MetadataOnly(MetadataOnlyReason::Oversized)
                    );
                    assert_eq!(loaded.text(), None);
                    assert_eq!(
                        loaded.content_hash(),
                        ContentHash::from_bytes(&[b'a'; 64])
                    );
                }
                ".keep" => {}
                other => panic!("unexpected path {other}"),
            }
        }
        assert!(seen_rs && seen_bin && seen_huge);
    }

    #[test]
    fn load_file_does_not_follow_symlink() {
        let ws = TempWorkspace::new();
        ws.write_file("core/src/lib.rs", b"ok");
        let outside = ws.write_file("outside/secret.rs", b"secret");
        let leak = ws.path.join("core/leak.rs");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &leak).expect("symlink");
            let err = load_file_candidate(
                &ws.path.join("core"),
                &walk_one_named(&ws, "src/lib.rs").0,
                &ContentLimits::default(),
                &CancellationToken::new(),
            );
            // Regular file still loads. Symlink path is rejected if requested.
            assert!(err.is_ok());
            let leak_path = path("leak.rs");
            let resolved = resolve_regular_file(&ws.path.join("core"), &leak_path);
            assert_eq!(resolved.err(), Some(ContentError::NotRegularFile));
        }
        let _ = (outside, leak);
    }

    fn walk_one_named(ws: &TempWorkspace, want: &str) -> (FileCandidate, WorkspaceManifest) {
        let manifest = parse_manifest(ws);
        let repo = manifest.repo_by_alias("core").expect("repo");
        let limits = WalkLimits::default();
        let cancel = CancellationToken::new();
        for item in walk_repo(repo, &limits, &cancel) {
            let candidate = item.expect("candidate");
            if candidate.path().as_str() == want {
                return (candidate, manifest);
            }
        }
        panic!("missing {want}");
    }
}
