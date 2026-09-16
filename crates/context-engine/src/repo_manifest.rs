//! Multi-repository workspace manifest and stable repository identities.
//!
//! Aliases are unique. Roots are canonicalized host directories. `RepoId` is
//! derived from the canonical root so an alias rename does not change identity.
//! Nested or duplicate roots are rejected because file ownership would be
//! ambiguous. Read-only repos cannot be selected as a write target.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::RepoId;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// Workspace manifest schema version.
pub const MANIFEST_SCHEMA: u16 = 1;

/// Maximum UTF-8 bytes accepted in a manifest document.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Maximum repositories accepted in one manifest.
pub const MAX_REPOS: usize = 64;

/// Maximum UTF-8 bytes accepted in a repository alias.
pub const MAX_ALIAS_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted in a raw repository root string.
pub const MAX_ROOT_BYTES: usize = 4096;

const IDENTITY_NAMESPACE: &[u8] = b"rapidlm.repo-identity.v1\0";
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Cooperative cancellation for manifest parse.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Typed parse/selection failure. Display never echoes raw root paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestError {
    Cancelled,
    SourceTooLarge,
    TooManyRepos,
    InvalidSyntax,
    UnsupportedSchema { found: u16 },
    MissingField { field: &'static str },
    InvalidAlias,
    DuplicateAlias { alias: String },
    NestedAlias { alias: String, other: String },
    InvalidRoot,
    RootNotFound { alias: String },
    RootNotDirectory { alias: String },
    DuplicateRoot { alias: String, other: String },
    NestedRoot { inner: String, outer: String },
    WorkspaceRootInvalid,
    UnknownRepo,
    ReadOnlyWriteTarget { alias: String },
    InternalIdentity,
}

/// Read/write policy attached to one repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RepoAccessMode {
    ReadOnly,
    ReadWrite,
}

/// Validated repository alias. Wire form is the alias string.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RepoAlias(String);

/// Canonical host directory for a repository root.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CanonicalRoot(PathBuf);

/// One attached repository after validation and identity assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoSpec {
    id: RepoId,
    alias: RepoAlias,
    root: CanonicalRoot,
    mode: RepoAccessMode,
}

/// Parsed multi-repo workspace manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceManifest {
    repos: Vec<RepoSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema: Option<u16>,
    repos: Option<Vec<RawRepo>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepo {
    alias: Option<String>,
    root: Option<String>,
    mode: Option<String>,
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct RootInode {
    dev: u64,
    ino: u64,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), ManifestError> {
        if self.is_cancelled() {
            Err(ManifestError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl RepoAccessMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }

    pub const fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

impl fmt::Display for RepoAccessMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RepoAccessMode {
    type Err = ManifestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read_only" => Ok(Self::ReadOnly),
            "read_write" => Ok(Self::ReadWrite),
            _ => Err(ManifestError::InvalidSyntax),
        }
    }
}

impl Serialize for RepoAccessMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RepoAccessMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse()
            .map_err(|_| D::Error::unknown_variant(&raw, &["read_only", "read_write"]))
    }
}

impl RepoAlias {
    pub fn parse(input: &str) -> Result<Self, ManifestError> {
        if input.is_empty() || input.len() > MAX_ALIAS_BYTES {
            return Err(ManifestError::InvalidAlias);
        }
        if input.contains('\0') || input.chars().any(char::is_control) {
            return Err(ManifestError::InvalidAlias);
        }
        if input.contains('/') || input.contains('\\') {
            return Err(ManifestError::NestedAlias {
                alias: input.to_owned(),
                other: input.to_owned(),
            });
        }
        if input == "." || input == ".." {
            return Err(ManifestError::InvalidAlias);
        }
        let mut chars = input.chars();
        let Some(first) = chars.next() else {
            return Err(ManifestError::InvalidAlias);
        };
        if !first.is_ascii_alphanumeric() {
            return Err(ManifestError::InvalidAlias);
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err(ManifestError::InvalidAlias);
        }
        Ok(Self(input.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RepoAlias {
    type Err = ManifestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl AsRef<str> for RepoAlias {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for RepoAlias {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl CanonicalRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for CanonicalRoot {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl RepoSpec {
    pub fn id(&self) -> RepoId {
        self.id
    }

    pub fn alias(&self) -> &RepoAlias {
        &self.alias
    }

    pub fn root(&self) -> &CanonicalRoot {
        &self.root
    }

    pub fn mode(&self) -> RepoAccessMode {
        self.mode
    }

    pub fn is_writable(&self) -> bool {
        self.mode.is_writable()
    }
}

impl WorkspaceManifest {
    /// Parse a TOML workspace manifest and assign stable identities.
    pub fn parse(
        src: &str,
        workspace_root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self, ManifestError> {
        cancel.check()?;
        if src.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::SourceTooLarge);
        }
        let raw: RawManifest = toml::from_str(src).map_err(|_| ManifestError::InvalidSyntax)?;
        cancel.check()?;
        let schema = raw
            .schema
            .ok_or(ManifestError::MissingField { field: "schema" })?;
        if schema != MANIFEST_SCHEMA {
            return Err(ManifestError::UnsupportedSchema { found: schema });
        }
        let repos = raw
            .repos
            .ok_or(ManifestError::MissingField { field: "repos" })?;
        Self::from_raw_repos(repos, workspace_root, cancel)
    }

    pub fn repos(&self) -> &[RepoSpec] {
        &self.repos
    }

    pub fn repo_by_alias(&self, alias: &str) -> Option<&RepoSpec> {
        self.repos.iter().find(|repo| repo.alias.as_str() == alias)
    }

    pub fn repo_by_id(&self, id: RepoId) -> Option<&RepoSpec> {
        self.repos.iter().find(|repo| repo.id == id)
    }

    /// Select a writable repository by alias. Read-only targets are rejected.
    pub fn write_target(&self, alias: &str) -> Result<&RepoSpec, ManifestError> {
        let repo = self
            .repo_by_alias(alias)
            .ok_or(ManifestError::UnknownRepo)?;
        ensure_writable(repo)
    }

    /// Select a writable repository by stable identity. Read-only targets are rejected.
    pub fn write_target_by_id(&self, id: RepoId) -> Result<&RepoSpec, ManifestError> {
        let repo = self.repo_by_id(id).ok_or(ManifestError::UnknownRepo)?;
        ensure_writable(repo)
    }

    fn from_raw_repos(
        raw_repos: Vec<RawRepo>,
        workspace_root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self, ManifestError> {
        cancel.check()?;
        if raw_repos.len() > MAX_REPOS {
            return Err(ManifestError::TooManyRepos);
        }
        let workspace_root = canonicalize_workspace(workspace_root)?;
        let mut repos = Vec::with_capacity(raw_repos.len());
        for raw in raw_repos {
            cancel.check()?;
            repos.push(validate_repo(raw, &workspace_root)?);
        }
        reject_ambiguous_aliases(&repos)?;
        reject_ambiguous_roots(&repos)?;
        Ok(Self { repos })
    }
}

fn ensure_writable(repo: &RepoSpec) -> Result<&RepoSpec, ManifestError> {
    if repo.mode.is_writable() {
        Ok(repo)
    } else {
        Err(ManifestError::ReadOnlyWriteTarget {
            alias: repo.alias.as_str().to_owned(),
        })
    }
}

fn canonicalize_workspace(workspace_root: &Path) -> Result<PathBuf, ManifestError> {
    let canonical = protocol::host_path::canonicalize(workspace_root)
        .map_err(|_| ManifestError::WorkspaceRootInvalid)?;
    if !canonical.is_dir() {
        return Err(ManifestError::WorkspaceRootInvalid);
    }
    Ok(canonical)
}

fn validate_repo(raw: RawRepo, workspace_root: &Path) -> Result<RepoSpec, ManifestError> {
    let alias_raw = raw
        .alias
        .ok_or(ManifestError::MissingField { field: "alias" })?;
    let root_raw = raw
        .root
        .ok_or(ManifestError::MissingField { field: "root" })?;
    let mode_raw = raw
        .mode
        .ok_or(ManifestError::MissingField { field: "mode" })?;
    let alias = RepoAlias::parse(&alias_raw)?;
    let mode = RepoAccessMode::from_str(&mode_raw).map_err(|_| ManifestError::InvalidSyntax)?;
    let root = canonicalize_root(&root_raw, workspace_root, alias.as_str())?;
    let id = stable_repo_id(root.as_path())?;
    Ok(RepoSpec {
        id,
        alias,
        root,
        mode,
    })
}

fn canonicalize_root(
    raw: &str,
    workspace_root: &Path,
    alias: &str,
) -> Result<CanonicalRoot, ManifestError> {
    if raw.is_empty() || raw.len() > MAX_ROOT_BYTES {
        return Err(ManifestError::InvalidRoot);
    }
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(ManifestError::InvalidRoot);
    }
    let joined = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workspace_root.join(raw)
    };
    let canonical =
        protocol::host_path::canonicalize(&joined).map_err(|_| ManifestError::RootNotFound {
            alias: alias.to_owned(),
        })?;
    if !canonical.is_dir() {
        return Err(ManifestError::RootNotDirectory {
            alias: alias.to_owned(),
        });
    }
    Ok(CanonicalRoot(canonical))
}

fn reject_ambiguous_aliases(repos: &[RepoSpec]) -> Result<(), ManifestError> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for repo in repos {
        let alias = repo.alias.as_str();
        let key = alias.to_ascii_lowercase();
        if let Some(existing) = seen.get(&key) {
            return Err(ManifestError::DuplicateAlias {
                alias: (*existing).to_owned(),
            });
        }
        seen.insert(key, alias);
    }
    Ok(())
}

fn reject_ambiguous_roots(repos: &[RepoSpec]) -> Result<(), ManifestError> {
    #[cfg(unix)]
    {
        let mut inodes: BTreeMap<RootInode, &str> = BTreeMap::new();
        for repo in repos {
            if let Some(inode) = root_inode(repo.root.as_path()) {
                if let Some(existing) = inodes.get(&inode) {
                    return Err(ManifestError::DuplicateRoot {
                        alias: repo.alias.as_str().to_owned(),
                        other: (*existing).to_owned(),
                    });
                }
                inodes.insert(inode, repo.alias.as_str());
            }
        }
    }

    for (i, left) in repos.iter().enumerate() {
        for right in repos.iter().skip(i + 1) {
            let left_path = left.root.as_path();
            let right_path = right.root.as_path();
            if left_path == right_path {
                return Err(ManifestError::DuplicateRoot {
                    alias: right.alias.as_str().to_owned(),
                    other: left.alias.as_str().to_owned(),
                });
            }
            if right_path.starts_with(left_path) {
                return Err(ManifestError::NestedRoot {
                    inner: right.alias.as_str().to_owned(),
                    outer: left.alias.as_str().to_owned(),
                });
            }
            if left_path.starts_with(right_path) {
                return Err(ManifestError::NestedRoot {
                    inner: left.alias.as_str().to_owned(),
                    outer: right.alias.as_str().to_owned(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn root_inode(path: &Path) -> Option<RootInode> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(RootInode {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

fn stable_repo_id(root: &Path) -> Result<RepoId, ManifestError> {
    let mut hasher = Sha256::new();
    hasher.update(IDENTITY_NAMESPACE);
    hasher.update(root.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    uuid8_from_bytes(&digest[..16])
}

fn uuid8_from_bytes(bytes: &[u8]) -> Result<RepoId, ManifestError> {
    if bytes.len() != 16 {
        return Err(ManifestError::InternalIdentity);
    }
    let mut raw = [0u8; 16];
    raw.copy_from_slice(bytes);
    raw[6] = (raw[6] & 0x0f) | 0x80;
    raw[8] = (raw[8] & 0x3f) | 0x80;
    let hex = hex_lower(&raw);
    let encoded = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    encoded.parse().map_err(|_| ManifestError::InternalIdentity)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("workspace manifest parse cancelled"),
            Self::SourceTooLarge => {
                write!(f, "workspace manifest exceeds {MAX_MANIFEST_BYTES} bytes")
            }
            Self::TooManyRepos => write!(f, "workspace manifest exceeds {MAX_REPOS} repositories"),
            Self::InvalidSyntax => f.write_str("invalid workspace manifest syntax"),
            Self::UnsupportedSchema { found } => {
                write!(f, "unsupported workspace manifest schema {found}")
            }
            Self::MissingField { field } => {
                write!(f, "workspace manifest missing field {field}")
            }
            Self::InvalidAlias => f.write_str("invalid repository alias"),
            Self::DuplicateAlias { alias } => {
                write!(f, "duplicate repository alias {alias}")
            }
            Self::NestedAlias { alias, other } => {
                write!(
                    f,
                    "nested or path-like repository alias {alias} conflicts with {other}"
                )
            }
            Self::InvalidRoot => f.write_str("invalid repository root"),
            Self::RootNotFound { alias } => {
                write!(f, "repository root for alias {alias} was not found")
            }
            Self::RootNotDirectory { alias } => {
                write!(f, "repository root for alias {alias} is not a directory")
            }
            Self::DuplicateRoot { alias, other } => {
                write!(
                    f,
                    "duplicate repository root for aliases {alias} and {other}"
                )
            }
            Self::NestedRoot { inner, outer } => {
                write!(
                    f,
                    "nested repository root {inner} is inside {outer} and is ambiguous"
                )
            }
            Self::WorkspaceRootInvalid => f.write_str("workspace root is not a usable directory"),
            Self::UnknownRepo => f.write_str("unknown repository"),
            Self::ReadOnlyWriteTarget { alias } => {
                write!(f, "read-only repository {alias} cannot be a write target")
            }
            Self::InternalIdentity => f.write_str("failed to assign a stable repository identity"),
        }
    }
}

impl Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const GOLDEN_MODE_READ_ONLY: &str = "\"read_only\"";
    const GOLDEN_MODE_READ_WRITE: &str = "\"read_write\"";

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
                "rapidlm-repo-manifest-{}-{nanos}-{seq}",
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

    fn parse(src: &str, root: &Path) -> Result<WorkspaceManifest, ManifestError> {
        WorkspaceManifest::parse(src, root, &CancellationToken::new())
    }

    fn two_repo_manifest(writable: &str, readonly: &str) -> String {
        format!(
            r#"
schema = 1

[[repos]]
alias = "core"
root = "{writable}"
mode = "read_write"

[[repos]]
alias = "docs"
root = "{readonly}"
mode = "read_only"
"#
        )
    }

    #[test]
    fn parse_assigns_stable_ids_and_canonical_roots() {
        let ws = TempWorkspace::new();
        let core = ws.mkdir("core");
        let docs = ws.mkdir("docs");
        let src = two_repo_manifest("core", "docs");
        let first = parse(&src, &ws.path).expect("parse");
        let second = parse(&src, &ws.path).expect("parse again");
        assert_eq!(first, second);
        assert_eq!(first.repos().len(), 2);

        let core_spec = first.repo_by_alias("core").expect("core");
        let docs_spec = first.repo_by_alias("docs").expect("docs");
        assert_eq!(core_spec.mode(), RepoAccessMode::ReadWrite);
        assert_eq!(docs_spec.mode(), RepoAccessMode::ReadOnly);
        assert_eq!(
            core_spec.root().as_path(),
            protocol::host_path::canonicalize(&core)
                .expect("canon core")
                .as_path()
        );
        assert_eq!(
            docs_spec.root().as_path(),
            protocol::host_path::canonicalize(&docs)
                .expect("canon docs")
                .as_path()
        );
        assert_ne!(core_spec.id(), docs_spec.id());
        assert_eq!(
            first.repo_by_id(core_spec.id()).map(RepoSpec::alias),
            Some(core_spec.alias())
        );
    }

    #[test]
    fn identity_is_stable_across_alias_rename() {
        let ws = TempWorkspace::new();
        ws.mkdir("core");
        let a = parse(
            r#"
schema = 1
[[repos]]
alias = "core"
root = "core"
mode = "read_write"
"#,
            &ws.path,
        )
        .expect("parse a");
        let b = parse(
            r#"
schema = 1
[[repos]]
alias = "main"
root = "./core/."
mode = "read_only"
"#,
            &ws.path,
        )
        .expect("parse b");
        assert_eq!(a.repos()[0].id(), b.repos()[0].id());
        assert_eq!(a.repos()[0].root(), b.repos()[0].root());
    }

    #[test]
    fn write_target_rejects_read_only_repo() {
        let ws = TempWorkspace::new();
        ws.mkdir("core");
        ws.mkdir("docs");
        let manifest = parse(&two_repo_manifest("core", "docs"), &ws.path).expect("parse");
        let writable = manifest.write_target("core").expect("writable");
        assert_eq!(writable.alias().as_str(), "core");
        assert!(writable.is_writable());
        assert_eq!(
            manifest.write_target("docs"),
            Err(ManifestError::ReadOnlyWriteTarget {
                alias: "docs".to_owned()
            })
        );
        let docs_id = manifest.repo_by_alias("docs").expect("docs").id();
        assert_eq!(
            manifest.write_target_by_id(docs_id),
            Err(ManifestError::ReadOnlyWriteTarget {
                alias: "docs".to_owned()
            })
        );
        assert_eq!(
            manifest.write_target("missing"),
            Err(ManifestError::UnknownRepo)
        );
        assert_eq!(
            manifest.write_target_by_id(RepoId::new()),
            Err(ManifestError::UnknownRepo)
        );
    }

    #[test]
    fn duplicate_alias_is_rejected() {
        let ws = TempWorkspace::new();
        ws.mkdir("a");
        ws.mkdir("b");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "core"
root = "a"
mode = "read_write"
[[repos]]
alias = "core"
root = "b"
mode = "read_only"
"#,
            &ws.path,
        )
        .expect_err("duplicate");
        assert_eq!(
            err,
            ManifestError::DuplicateAlias {
                alias: "core".to_owned()
            }
        );
        assert_eq!(err.to_string(), "duplicate repository alias core");
    }

    #[test]
    fn case_variant_alias_is_duplicate() {
        let ws = TempWorkspace::new();
        ws.mkdir("a");
        ws.mkdir("b");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "Core"
root = "a"
mode = "read_write"
[[repos]]
alias = "core"
root = "b"
mode = "read_only"
"#,
            &ws.path,
        )
        .expect_err("case duplicate");
        assert_eq!(
            err,
            ManifestError::DuplicateAlias {
                alias: "Core".to_owned()
            }
        );
    }

    #[test]
    fn path_like_alias_is_nested_ambiguous() {
        let ws = TempWorkspace::new();
        ws.mkdir("a");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "libs/core"
root = "a"
mode = "read_write"
"#,
            &ws.path,
        )
        .expect_err("nested alias");
        assert_eq!(
            err,
            ManifestError::NestedAlias {
                alias: "libs/core".to_owned(),
                other: "libs/core".to_owned()
            }
        );
        assert!(err.to_string().contains("nested"));
        assert_eq!(
            RepoAlias::parse("libs\\core").expect_err("backslash"),
            ManifestError::NestedAlias {
                alias: "libs\\core".to_owned(),
                other: "libs\\core".to_owned()
            }
        );
    }

    #[test]
    fn nested_root_is_rejected() {
        let ws = TempWorkspace::new();
        ws.mkdir("outer");
        ws.mkdir("outer/inner");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "outer"
root = "outer"
mode = "read_only"
[[repos]]
alias = "inner"
root = "outer/inner"
mode = "read_write"
"#,
            &ws.path,
        )
        .expect_err("nested root");
        assert_eq!(
            err,
            ManifestError::NestedRoot {
                inner: "inner".to_owned(),
                outer: "outer".to_owned()
            }
        );
        assert!(err.to_string().contains("ambiguous"));
        assert!(!err.to_string().contains("outer/inner"));
    }

    #[test]
    fn sibling_roots_with_shared_prefix_are_not_nested() {
        let ws = TempWorkspace::new();
        ws.mkdir("core");
        ws.mkdir("core-extra");
        let manifest = parse(
            r#"
schema = 1
[[repos]]
alias = "core"
root = "core"
mode = "read_write"
[[repos]]
alias = "core_extra"
root = "core-extra"
mode = "read_only"
"#,
            &ws.path,
        )
        .expect("siblings");
        assert_eq!(manifest.repos().len(), 2);
    }

    #[test]
    fn duplicate_canonical_root_is_rejected() {
        let ws = TempWorkspace::new();
        ws.mkdir("core");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "a"
root = "core"
mode = "read_write"
[[repos]]
alias = "b"
root = "./core/."
mode = "read_only"
"#,
            &ws.path,
        )
        .expect_err("duplicate root");
        assert_eq!(
            err,
            ManifestError::DuplicateRoot {
                alias: "b".to_owned(),
                other: "a".to_owned()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_into_another_repo_is_nested_or_duplicate() {
        let ws = TempWorkspace::new();
        ws.mkdir("primary");
        let link = ws.path.join("shadow");
        std::os::unix::fs::symlink(ws.path.join("primary"), &link).expect("symlink");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "alpha"
root = "primary"
mode = "read_only"
[[repos]]
alias = "beta"
root = "shadow"
mode = "read_write"
"#,
            &ws.path,
        )
        .expect_err("symlink alias");
        assert!(
            matches!(
                err,
                ManifestError::DuplicateRoot { .. } | ManifestError::NestedRoot { .. }
            ),
            "symlink write-target bypass: {err:?}"
        );
        let shown = err.to_string();
        assert!(!shown.contains("shadow"));
        assert!(!shown.contains(ws.path.to_string_lossy().as_ref()));
    }

    #[test]
    fn file_root_is_rejected() {
        let ws = TempWorkspace::new();
        ws.write_file("not-a-dir", b"x");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "core"
root = "not-a-dir"
mode = "read_write"
"#,
            &ws.path,
        )
        .expect_err("file root");
        assert_eq!(
            err,
            ManifestError::RootNotDirectory {
                alias: "core".to_owned()
            }
        );
    }

    #[test]
    fn missing_root_is_rejected() {
        let ws = TempWorkspace::new();
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "core"
root = "missing"
mode = "read_write"
"#,
            &ws.path,
        )
        .expect_err("missing");
        assert_eq!(
            err,
            ManifestError::RootNotFound {
                alias: "core".to_owned()
            }
        );
    }

    #[test]
    fn cancelled_parse_fails_closed() {
        let ws = TempWorkspace::new();
        ws.mkdir("core");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            WorkspaceManifest::parse(
                r#"
schema = 1
[[repos]]
alias = "core"
root = "core"
mode = "read_write"
"#,
                &ws.path,
                &cancel
            ),
            Err(ManifestError::Cancelled)
        );
    }

    #[test]
    fn oversized_manifest_is_rejected() {
        let ws = TempWorkspace::new();
        let body = "x".repeat(MAX_MANIFEST_BYTES + 1);
        assert_eq!(parse(&body, &ws.path), Err(ManifestError::SourceTooLarge));
    }

    #[test]
    fn too_many_repos_is_rejected() {
        let ws = TempWorkspace::new();
        let mut src = String::from("schema = 1\n");
        for i in 0..=MAX_REPOS {
            let name = format!("r{i}");
            ws.mkdir(&name);
            src.push_str(&format!(
                "[[repos]]\nalias = \"{name}\"\nroot = \"{name}\"\nmode = \"read_only\"\n"
            ));
        }
        assert_eq!(parse(&src, &ws.path), Err(ManifestError::TooManyRepos));
    }

    #[test]
    fn unknown_mode_and_schema_are_rejected() {
        let ws = TempWorkspace::new();
        ws.mkdir("core");
        assert_eq!(
            parse(
                r#"
schema = 2
[[repos]]
alias = "core"
root = "core"
mode = "read_write"
"#,
                &ws.path
            ),
            Err(ManifestError::UnsupportedSchema { found: 2 })
        );
        assert_eq!(
            parse(
                r#"
schema = 1
[[repos]]
alias = "core"
root = "core"
mode = "write"
"#,
                &ws.path
            ),
            Err(ManifestError::InvalidSyntax)
        );
    }

    #[test]
    fn access_mode_golden_json_round_trips() {
        let read_only = serde_json::to_string(&RepoAccessMode::ReadOnly).expect("ser");
        let read_write = serde_json::to_string(&RepoAccessMode::ReadWrite).expect("ser");
        assert_eq!(read_only, GOLDEN_MODE_READ_ONLY);
        assert_eq!(read_write, GOLDEN_MODE_READ_WRITE);
        let decoded_ro: RepoAccessMode = serde_json::from_str(GOLDEN_MODE_READ_ONLY).expect("de");
        let decoded_rw: RepoAccessMode = serde_json::from_str(GOLDEN_MODE_READ_WRITE).expect("de");
        assert_eq!(decoded_ro, RepoAccessMode::ReadOnly);
        assert_eq!(decoded_rw, RepoAccessMode::ReadWrite);
        assert!(serde_json::from_str::<RepoAccessMode>("\"READ_ONLY\"").is_err());
        assert!(serde_json::from_str::<RepoAccessMode>("\"rw\"").is_err());
    }

    #[test]
    fn alias_rejects_traversal_and_controls() {
        for sample in ["", ".", "..", "has space", "bad:alias", "/abs", "\ncore"] {
            assert_eq!(
                RepoAlias::parse(sample),
                Err(if sample.contains('/') {
                    ManifestError::NestedAlias {
                        alias: sample.to_owned(),
                        other: sample.to_owned(),
                    }
                } else {
                    ManifestError::InvalidAlias
                }),
                "accepted {sample:?}"
            );
        }
        let ok = RepoAlias::parse("core_lib.v1").expect("valid");
        assert_eq!(ok.as_str(), "core_lib.v1");
        assert_eq!(serde_json::to_string(&ok).expect("ser"), "\"core_lib.v1\"");
    }

    #[test]
    fn error_display_does_not_echo_root_path() {
        let ws = TempWorkspace::new();
        ws.mkdir("secret-root");
        ws.mkdir("secret-root/nested");
        let err = parse(
            r#"
schema = 1
[[repos]]
alias = "outer"
root = "secret-root"
mode = "read_only"
[[repos]]
alias = "inner"
root = "secret-root/nested"
mode = "read_write"
"#,
            &ws.path,
        )
        .expect_err("nested");
        let shown = err.to_string();
        assert!(!shown.contains("secret-root"));
        assert!(!shown.contains(ws.path.to_string_lossy().as_ref()));
    }
}
