//! Filesystem action normalizer.
//!
//! Resolves repo/host paths, symlink/junction chains, and the intended
//! operation into a stable canonical identity *before* policy hashing.
//! Untrusted intent is never rewritten into an in-scope path: traversal
//! fails closed, and symlink/junction escape cannot produce a repo grant.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use protocol::{RepoPath, RepoPathError};

use super::command::{CancellationToken, CanonicalHostPath, CommandNormalizeError};
use crate::capability::FilesystemRoot;

/// Maximum UTF-8 bytes for a requested or resolved path.
pub const MAX_FS_PATH_BYTES: usize = 4096;

/// Maximum symlink/junction hops followed for one target.
pub const MAX_SYMLINK_HOPS: usize = 32;

const POLICY_TAG: &[u8] = b"rapidlm.canonical_fs.v1";
const CANCEL_STRIDE: usize = 16;

/// Untrusted filesystem request. Model/tool supplied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsIntent {
    root: FilesystemRoot,
    operation: FsOperation,
}

/// Intended filesystem operation. Rename carries two untrusted paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FsOperation {
    Read { path: String },
    Write { path: String },
    Create { path: String },
    Delete { path: String },
    Rename { from: String, to: String },
}

/// Frozen operation class used in the action hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FsOpKind {
    Read,
    Write,
    Create,
    Delete,
    Rename,
}

/// Canonical file identity after symlink resolution and confinement.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum CanonicalFsIdentity {
    Repo(RepoPath),
    Host(CanonicalHostPath),
}

/// One resolved target. `existed` is a snapshot and is not hashed.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CanonicalFsTarget {
    identity: CanonicalFsIdentity,
    existed: bool,
}

/// Normalized filesystem action bound into policy hashing.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CanonicalFsAction {
    root: FilesystemRoot,
    operation: FsOpKind,
    path: CanonicalFsTarget,
    dest: Option<CanonicalFsTarget>,
}

/// Resolves existence, directory-ness, and symlink/junction targets.
///
/// Implementations fail closed on I/O ambiguity. Search paths and host
/// identity come from this trusted resolver, never from the intent.
pub trait FsResolver {
    fn repo_root(&self) -> &CanonicalHostPath;

    fn host_base(&self) -> &CanonicalHostPath;

    fn exists(&self, path: &CanonicalHostPath) -> Result<bool, FsNormalizeError>;

    fn is_dir(&self, path: &CanonicalHostPath) -> Result<bool, FsNormalizeError>;

    /// `Ok(Some(target))` when `path` is a symlink or junction.
    fn read_link(&self, path: &CanonicalHostPath) -> Result<Option<String>, FsNormalizeError>;
}

/// Typed normalize failure. Display never echoes attacker-controlled input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsNormalizeError {
    Cancelled,
    EmptyPath,
    TooLong,
    Nul,
    Control,
    Unc,
    Traversal,
    AbsolutePath,
    WindowsDrive,
    Escape,
    UnresolvedPath,
    UnresolvedParent,
    NotADirectory,
    SymlinkLoop,
    GitScopeRequired,
}

impl FsIntent {
    pub fn read(root: FilesystemRoot, path: impl Into<String>) -> Self {
        Self {
            root,
            operation: FsOperation::Read { path: path.into() },
        }
    }

    pub fn write(root: FilesystemRoot, path: impl Into<String>) -> Self {
        Self {
            root,
            operation: FsOperation::Write { path: path.into() },
        }
    }

    pub fn create(root: FilesystemRoot, path: impl Into<String>) -> Self {
        Self {
            root,
            operation: FsOperation::Create { path: path.into() },
        }
    }

    pub fn delete(root: FilesystemRoot, path: impl Into<String>) -> Self {
        Self {
            root,
            operation: FsOperation::Delete { path: path.into() },
        }
    }

    pub fn rename(root: FilesystemRoot, from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            root,
            operation: FsOperation::Rename {
                from: from.into(),
                to: to.into(),
            },
        }
    }

    pub fn root(&self) -> FilesystemRoot {
        self.root
    }

    pub fn operation(&self) -> &FsOperation {
        &self.operation
    }
}

impl FsOpKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Rename => "rename",
        }
    }

    pub const fn is_mutating(self) -> bool {
        !matches!(self, Self::Read)
    }
}

impl CanonicalFsIdentity {
    pub fn root(&self) -> FilesystemRoot {
        match self {
            Self::Repo(_) => FilesystemRoot::Repo,
            Self::Host(_) => FilesystemRoot::Host,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Repo(path) => path.as_str(),
            Self::Host(path) => path.as_str(),
        }
    }

    pub fn repo(&self) -> Option<&RepoPath> {
        match self {
            Self::Repo(path) => Some(path),
            Self::Host(_) => None,
        }
    }

    pub fn host(&self) -> Option<&CanonicalHostPath> {
        match self {
            Self::Host(path) => Some(path),
            Self::Repo(_) => None,
        }
    }
}

impl CanonicalFsTarget {
    pub fn identity(&self) -> &CanonicalFsIdentity {
        &self.identity
    }

    pub fn existed(&self) -> bool {
        self.existed
    }

    pub fn as_str(&self) -> &str {
        self.identity.as_str()
    }
}

impl CanonicalFsAction {
    pub fn root(&self) -> FilesystemRoot {
        self.root
    }

    pub fn operation(&self) -> FsOpKind {
        self.operation
    }

    pub fn path(&self) -> &CanonicalFsTarget {
        &self.path
    }

    pub fn dest(&self) -> Option<&CanonicalFsTarget> {
        self.dest.as_ref()
    }

    /// Stable bytes for action-hash input. Existence snapshots are omitted.
    pub fn policy_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.path.as_str().len());
        out.extend_from_slice(POLICY_TAG);
        out.push(0);
        out.extend_from_slice(self.root.as_str().as_bytes());
        out.push(0);
        out.extend_from_slice(self.operation.as_str().as_bytes());
        out.push(0);
        out.extend_from_slice(self.path.as_str().as_bytes());
        out.push(0);
        if let Some(dest) = &self.dest {
            out.extend_from_slice(dest.as_str().as_bytes());
            out.push(0);
        }
        out
    }
}

/// Resolve repo/host paths, symlink/junction chains, and the intended operation.
pub fn normalize_fs<R: FsResolver + ?Sized>(
    intent: &FsIntent,
    resolver: &R,
    cancel: &CancellationToken,
) -> Result<CanonicalFsAction, FsNormalizeError> {
    cancel_check(cancel)?;
    let kind = op_kind(&intent.operation);
    match &intent.operation {
        FsOperation::Read { path }
        | FsOperation::Write { path }
        | FsOperation::Create { path }
        | FsOperation::Delete { path } => {
            let target = resolve_target(intent.root, path, kind, Side::Primary, resolver, cancel)?;
            reject_git_mutation(kind, &target, resolver.repo_root())?;
            Ok(CanonicalFsAction {
                root: intent.root,
                operation: kind,
                path: target,
                dest: None,
            })
        }
        FsOperation::Rename { from, to } => {
            let src = resolve_target(intent.root, from, kind, Side::Primary, resolver, cancel)?;
            cancel_check(cancel)?;
            let dest = resolve_target(intent.root, to, kind, Side::Dest, resolver, cancel)?;
            reject_git_mutation(kind, &src, resolver.repo_root())?;
            reject_git_mutation(kind, &dest, resolver.repo_root())?;
            Ok(CanonicalFsAction {
                root: intent.root,
                operation: kind,
                path: src,
                dest: Some(dest),
            })
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Primary,
    Dest,
}

struct WalkRules {
    missing_ok: bool,
    follow_last: bool,
}

fn op_kind(op: &FsOperation) -> FsOpKind {
    match op {
        FsOperation::Read { .. } => FsOpKind::Read,
        FsOperation::Write { .. } => FsOpKind::Write,
        FsOperation::Create { .. } => FsOpKind::Create,
        FsOperation::Delete { .. } => FsOpKind::Delete,
        FsOperation::Rename { .. } => FsOpKind::Rename,
    }
}

fn walk_rules(kind: FsOpKind, side: Side) -> WalkRules {
    match (kind, side) {
        (FsOpKind::Read, _) => WalkRules {
            missing_ok: false,
            follow_last: true,
        },
        (FsOpKind::Write, _) => WalkRules {
            missing_ok: true,
            follow_last: true,
        },
        (FsOpKind::Create, _) => WalkRules {
            missing_ok: true,
            follow_last: false,
        },
        (FsOpKind::Delete, _) => WalkRules {
            missing_ok: false,
            follow_last: false,
        },
        (FsOpKind::Rename, Side::Primary) => WalkRules {
            missing_ok: false,
            follow_last: false,
        },
        (FsOpKind::Rename, Side::Dest) => WalkRules {
            missing_ok: true,
            follow_last: false,
        },
    }
}

fn resolve_target<R: FsResolver + ?Sized>(
    root: FilesystemRoot,
    requested: &str,
    kind: FsOpKind,
    side: Side,
    resolver: &R,
    cancel: &CancellationToken,
) -> Result<CanonicalFsTarget, FsNormalizeError> {
    cancel_check(cancel)?;
    validate_requested_text(requested)?;
    let rules = walk_rules(kind, side);
    let confine = match root {
        FilesystemRoot::Repo => Some(resolver.repo_root().clone()),
        FilesystemRoot::Host => None,
    };
    let (walk_root, components) = match root {
        FilesystemRoot::Repo => {
            let repo = RepoPath::parse(requested).map_err(map_repo_path)?;
            let comps: Vec<String> = repo.components().map(str::to_owned).collect();
            reject_requested_git(
                kind,
                root,
                resolver.repo_root(),
                &comps,
                resolver.repo_root(),
            )?;
            (resolver.repo_root().clone(), comps)
        }
        FilesystemRoot::Host => {
            let (walk_root, comps) = split_host_request(requested, resolver.host_base())?;
            reject_requested_git(kind, root, &walk_root, &comps, resolver.repo_root())?;
            (walk_root, comps)
        }
    };
    let (resolved, existed) = walk(
        resolver,
        &walk_root,
        components,
        confine.as_ref(),
        rules,
        cancel,
    )?;
    confine_or_escape(&resolved, confine.as_ref())?;
    let identity = match root {
        FilesystemRoot::Repo => {
            let rel = strip_root(&resolved, resolver.repo_root())?;
            CanonicalFsIdentity::Repo(RepoPath::parse(rel).map_err(map_repo_path)?)
        }
        FilesystemRoot::Host => CanonicalFsIdentity::Host(resolved),
    };
    Ok(CanonicalFsTarget { identity, existed })
}

fn walk<R: FsResolver + ?Sized>(
    resolver: &R,
    start: &CanonicalHostPath,
    mut components: Vec<String>,
    confine: Option<&CanonicalHostPath>,
    rules: WalkRules,
    cancel: &CancellationToken,
) -> Result<(CanonicalHostPath, bool), FsNormalizeError> {
    let mut current = start.clone();
    let mut hops = 0usize;
    let mut seen = BTreeSet::new();
    let mut i = 0usize;
    while i < components.len() {
        if i.is_multiple_of(CANCEL_STRIDE) {
            cancel_check(cancel)?;
        }
        let last = i + 1 == components.len();
        let name = components[i].as_str();
        if name == ".." {
            current = parent_of(&current).ok_or(FsNormalizeError::Traversal)?;
            confine_or_escape(&current, confine)?;
            i += 1;
            continue;
        }
        if name.is_empty() || name == "." {
            i += 1;
            continue;
        }
        let candidate = canonical_host(&join_host(current.as_str(), name))?;
        confine_or_escape(&candidate, confine)?;
        // A symlink/junction is never a missing write/create leaf, even when
        // exists() is a follow-stat and the target is dangling.
        let link = resolver.read_link(&candidate)?;
        if let Some(target) = link {
            let follow = !last || rules.follow_last;
            if follow {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(FsNormalizeError::SymlinkLoop);
                }
                if !seen.insert(candidate.as_str().to_owned()) {
                    return Err(FsNormalizeError::SymlinkLoop);
                }
                let remaining = components[i + 1..].to_vec();
                let (next_root, next_comps) = splice_link(&current, &target, &remaining)?;
                if !is_walk_root(&next_root) {
                    confine_or_escape(&next_root, confine)?;
                }
                current = next_root;
                components = next_comps;
                i = 0;
                continue;
            }
            // Bind the link inode, but never mint an in-scope write/create
            // identity whose target is already known to escape confinement.
            if rules.missing_ok {
                let (next_root, next_comps) = splice_link(&current, &target, &[])?;
                reject_escaping_link_target(&next_root, &next_comps, confine)?;
            }
            return Ok((candidate, true));
        }
        let exists = resolver.exists(&candidate)?;
        if !exists {
            if last && rules.missing_ok {
                if !resolver.is_dir(&current)? {
                    return Err(FsNormalizeError::NotADirectory);
                }
                return Ok((candidate, false));
            }
            return Err(if last {
                FsNormalizeError::UnresolvedPath
            } else {
                FsNormalizeError::UnresolvedParent
            });
        }
        if !last && !resolver.is_dir(&candidate)? {
            return Err(FsNormalizeError::NotADirectory);
        }
        current = candidate;
        i += 1;
    }
    Ok((current, true))
}

fn reject_escaping_link_target(
    next_root: &CanonicalHostPath,
    next_comps: &[String],
    confine: Option<&CanonicalHostPath>,
) -> Result<(), FsNormalizeError> {
    let Some(root) = confine else {
        return Ok(());
    };
    if !is_walk_root(next_root) {
        confine_or_escape(next_root, confine)?;
    }
    let mut current = next_root.clone();
    for part in next_comps {
        if part == ".." {
            current = parent_of(&current).ok_or(FsNormalizeError::Traversal)?;
        } else if part.is_empty() || part == "." {
            continue;
        } else {
            current = canonical_host(&join_host(current.as_str(), part))?;
        }
        confine_or_escape(&current, Some(root))?;
    }
    Ok(())
}

fn splice_link(
    parent: &CanonicalHostPath,
    target: &str,
    remaining: &[String],
) -> Result<(CanonicalHostPath, Vec<String>), FsNormalizeError> {
    validate_requested_text(target)?;
    if target.is_empty() {
        return Err(FsNormalizeError::EmptyPath);
    }
    if is_unc(target) {
        return Err(FsNormalizeError::Unc);
    }
    let (root, mut comps) = if is_absolute_host(target) {
        split_absolute_host(target)?
    } else {
        let (root, mut prefix) = split_absolute_host(parent.as_str())?;
        for part in target.split(['/', '\\']) {
            if part.is_empty() || part == "." {
                continue;
            }
            prefix.push(part.to_owned());
        }
        (root, prefix)
    };
    comps.extend(remaining.iter().cloned());
    Ok((root, comps))
}

fn split_host_request(
    requested: &str,
    host_base: &CanonicalHostPath,
) -> Result<(CanonicalHostPath, Vec<String>), FsNormalizeError> {
    if requested.split(['/', '\\']).any(|part| part == "..") {
        return Err(FsNormalizeError::Traversal);
    }
    let absolute = is_absolute_host(requested);
    let joined = if absolute {
        requested.to_owned()
    } else {
        join_host(host_base.as_str(), requested)
    };
    split_absolute_host(&joined)
}

fn split_absolute_host(path: &str) -> Result<(CanonicalHostPath, Vec<String>), FsNormalizeError> {
    if is_unc(path) {
        return Err(FsNormalizeError::Unc);
    }
    let (drive, rest) = split_drive(path);
    let rooted = drive.is_some() || path.starts_with('/') || path.starts_with('\\');
    if !rooted {
        return Err(FsNormalizeError::UnresolvedPath);
    }
    let root = match drive {
        Some(d) => canonical_host(&format!("{d}/"))?,
        None => canonical_host("/")?,
    };
    let mut comps = Vec::new();
    for part in rest.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        comps.push(part.to_owned());
    }
    Ok((root, comps))
}

fn reject_git_mutation(
    kind: FsOpKind,
    target: &CanonicalFsTarget,
    repo_root: &CanonicalHostPath,
) -> Result<(), FsNormalizeError> {
    if !kind.is_mutating() {
        return Ok(());
    }
    if touches_git(&target.identity, repo_root) {
        return Err(FsNormalizeError::GitScopeRequired);
    }
    Ok(())
}

fn reject_requested_git(
    kind: FsOpKind,
    root: FilesystemRoot,
    walk_root: &CanonicalHostPath,
    components: &[String],
    repo_root: &CanonicalHostPath,
) -> Result<(), FsNormalizeError> {
    if !kind.is_mutating() {
        return Ok(());
    }
    match root {
        FilesystemRoot::Repo => {
            if components.iter().any(|part| is_git_component(part)) {
                return Err(FsNormalizeError::GitScopeRequired);
            }
        }
        FilesystemRoot::Host => {
            let mut path = walk_root.clone();
            for part in components {
                path = canonical_host(&join_host(path.as_str(), part))?;
            }
            if host_under_repo_git(path.as_str(), repo_root.as_str()) {
                return Err(FsNormalizeError::GitScopeRequired);
            }
        }
    }
    Ok(())
}

fn is_git_component(part: &str) -> bool {
    part.eq_ignore_ascii_case(".git")
}

fn touches_git(identity: &CanonicalFsIdentity, repo_root: &CanonicalHostPath) -> bool {
    match identity {
        CanonicalFsIdentity::Repo(path) => path.components().any(is_git_component),
        CanonicalFsIdentity::Host(path) => host_under_repo_git(path.as_str(), repo_root.as_str()),
    }
}

fn host_under_repo_git(path: &str, repo_root: &str) -> bool {
    if path == repo_root {
        return false;
    }
    let rel = if repo_root == "/" {
        match path.strip_prefix('/') {
            Some(rel) => rel,
            None => return false,
        }
    } else if is_drive_root(repo_root) {
        match path.strip_prefix(repo_root) {
            Some(rel) => rel,
            None => return false,
        }
    } else {
        let Some(rest) = path.strip_prefix(repo_root) else {
            return false;
        };
        match rest.strip_prefix('/') {
            Some(rel) => rel,
            None => return false,
        }
    };
    rel.split('/')
        .next()
        .is_some_and(|part| !part.is_empty() && is_git_component(part))
}

fn strip_root<'a>(
    path: &'a CanonicalHostPath,
    root: &CanonicalHostPath,
) -> Result<&'a str, FsNormalizeError> {
    let p = path.as_str();
    let r = root.as_str();
    if p == r {
        return Err(FsNormalizeError::EmptyPath);
    }
    let prefix = format!("{r}/");
    p.strip_prefix(&prefix).ok_or(FsNormalizeError::Escape)
}

fn confine_or_escape(
    path: &CanonicalHostPath,
    confine: Option<&CanonicalHostPath>,
) -> Result<(), FsNormalizeError> {
    match confine {
        Some(root) if !is_within(path, root) => Err(FsNormalizeError::Escape),
        _ => Ok(()),
    }
}

fn is_walk_root(path: &CanonicalHostPath) -> bool {
    path.as_str() == "/" || is_drive_root(path.as_str())
}

fn is_within(path: &CanonicalHostPath, root: &CanonicalHostPath) -> bool {
    let p = path.as_str();
    let r = root.as_str();
    if p == r {
        return true;
    }
    if r == "/" {
        return p.starts_with('/');
    }
    if is_drive_root(r) {
        return p.starts_with(r);
    }
    p.starts_with(r) && p[r.len()..].starts_with('/')
}

fn parent_of(path: &CanonicalHostPath) -> Option<CanonicalHostPath> {
    let raw = path.as_str();
    if raw == "/" || is_drive_root(raw) {
        return None;
    }
    match raw.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => canonical_host(parent).ok(),
        Some((_, _)) => canonical_host("/").ok(),
        None => None,
    }
}

fn join_host(base: &str, requested: &str) -> String {
    if requested.is_empty() {
        return base.to_owned();
    }
    if base == "/" {
        return format!("/{requested}");
    }
    if is_drive_root(base) {
        return format!("{base}{requested}");
    }
    format!("{base}/{requested}")
}

fn canonical_host(path: &str) -> Result<CanonicalHostPath, FsNormalizeError> {
    CanonicalHostPath::from_resolved(path).map_err(map_host_path)
}

fn validate_requested_text(path: &str) -> Result<(), FsNormalizeError> {
    if path.is_empty() {
        return Err(FsNormalizeError::EmptyPath);
    }
    if path.len() > MAX_FS_PATH_BYTES {
        return Err(FsNormalizeError::TooLong);
    }
    if path.contains('\0') {
        return Err(FsNormalizeError::Nul);
    }
    if path.chars().any(char::is_control) {
        return Err(FsNormalizeError::Control);
    }
    if is_unc(path) {
        return Err(FsNormalizeError::Unc);
    }
    Ok(())
}

fn is_absolute_host(path: &str) -> bool {
    path.starts_with('/') || path.starts_with('\\') || split_drive(path).0.is_some()
}

fn is_unc(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 2 && matches!(bytes[0], b'/' | b'\\') && matches!(bytes[1], b'/' | b'\\')
}

fn split_drive(path: &str) -> (Option<&str>, &str) {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        (Some(&path[..2]), &path[2..])
    } else {
        (None, path)
    }
}

fn is_drive_root(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() == 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), FsNormalizeError> {
    if cancel.is_cancelled() {
        Err(FsNormalizeError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_repo_path(err: RepoPathError) -> FsNormalizeError {
    match err {
        RepoPathError::Empty => FsNormalizeError::EmptyPath,
        RepoPathError::TooLong => FsNormalizeError::TooLong,
        RepoPathError::Nul => FsNormalizeError::Nul,
        RepoPathError::Control => FsNormalizeError::Control,
        RepoPathError::Absolute => FsNormalizeError::AbsolutePath,
        RepoPathError::WindowsDrive => FsNormalizeError::WindowsDrive,
        RepoPathError::Unc => FsNormalizeError::Unc,
        RepoPathError::Traversal => FsNormalizeError::Traversal,
    }
}

fn map_host_path(err: CommandNormalizeError) -> FsNormalizeError {
    match err {
        CommandNormalizeError::Cancelled => FsNormalizeError::Cancelled,
        CommandNormalizeError::EmptyCwd | CommandNormalizeError::EmptyExecutable => {
            FsNormalizeError::EmptyPath
        }
        CommandNormalizeError::TooLong => FsNormalizeError::TooLong,
        CommandNormalizeError::Nul => FsNormalizeError::Nul,
        CommandNormalizeError::Control => FsNormalizeError::Control,
        CommandNormalizeError::Unc => FsNormalizeError::Unc,
        CommandNormalizeError::Traversal => FsNormalizeError::Traversal,
        CommandNormalizeError::UnresolvedCwd | CommandNormalizeError::UnresolvedExecutable => {
            FsNormalizeError::UnresolvedPath
        }
        CommandNormalizeError::SymlinkLoop => FsNormalizeError::SymlinkLoop,
        _ => FsNormalizeError::UnresolvedPath,
    }
}

impl fmt::Display for FsOpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for FsNormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "filesystem normalization cancelled",
            Self::EmptyPath => "filesystem path is empty",
            Self::TooLong => "filesystem path exceeds bound",
            Self::Nul => "filesystem path contains NUL",
            Self::Control => "filesystem path contains a control character",
            Self::Unc => "UNC path is not a local filesystem path",
            Self::Traversal => "filesystem path contains a traversal component",
            Self::AbsolutePath => "repository path must be relative",
            Self::WindowsDrive => "Windows drive path is not a repository path",
            Self::Escape => "resolved filesystem path escapes the authorized root",
            Self::UnresolvedPath => "filesystem path could not be resolved",
            Self::UnresolvedParent => "filesystem parent path could not be resolved",
            Self::NotADirectory => "filesystem parent is not a directory",
            Self::SymlinkLoop => "filesystem symlink loop",
            Self::GitScopeRequired => "git directory writes require git.write, not fs.write",
        })
    }
}

impl Error for FsNormalizeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct LexicalFs {
        repo_root: CanonicalHostPath,
        host_base: CanonicalHostPath,
        files: BTreeSet<String>,
        dirs: BTreeSet<String>,
        links: BTreeMap<String, String>,
        dangling: BTreeSet<String>,
    }

    impl LexicalFs {
        fn new(repo_root: &str, host_base: &str) -> Self {
            let mut fs = Self {
                repo_root: CanonicalHostPath::from_resolved(repo_root).expect("repo root"),
                host_base: CanonicalHostPath::from_resolved(host_base).expect("host base"),
                files: BTreeSet::new(),
                dirs: BTreeSet::new(),
                links: BTreeMap::new(),
                dangling: BTreeSet::new(),
            };
            fs.dirs.insert(fs.repo_root.as_str().to_owned());
            fs.dirs.insert("/".to_owned());
            fs
        }

        fn with_file(mut self, path: &str) -> Self {
            self.add_parents(path);
            self.files.insert(path.to_owned());
            self
        }

        fn with_dir(mut self, path: &str) -> Self {
            self.add_parents(path);
            self.dirs.insert(path.to_owned());
            self
        }

        fn with_link(mut self, from: &str, to: &str) -> Self {
            self.add_parents(from);
            self.links.insert(from.to_owned(), to.to_owned());
            self
        }

        /// Symlink whose follow-stat `exists` is false (dangling target).
        fn with_dangling_link(mut self, from: &str, to: &str) -> Self {
            self.add_parents(from);
            self.links.insert(from.to_owned(), to.to_owned());
            self.dangling.insert(from.to_owned());
            self
        }

        fn add_parents(&mut self, path: &str) {
            let mut prefix = String::new();
            for part in path.split('/') {
                if part.is_empty() {
                    if prefix.is_empty() {
                        prefix.push('/');
                        self.dirs.insert("/".to_owned());
                    }
                    continue;
                }
                if prefix == "/" {
                    prefix = format!("/{part}");
                } else if prefix.is_empty() {
                    prefix = part.to_owned();
                } else {
                    prefix = format!("{prefix}/{part}");
                }
                if prefix != path {
                    self.dirs.insert(prefix.clone());
                }
            }
        }
    }

    impl FsResolver for LexicalFs {
        fn repo_root(&self) -> &CanonicalHostPath {
            &self.repo_root
        }

        fn host_base(&self) -> &CanonicalHostPath {
            &self.host_base
        }

        fn exists(&self, path: &CanonicalHostPath) -> Result<bool, FsNormalizeError> {
            let p = path.as_str();
            if self.dangling.contains(p) {
                return Ok(false);
            }
            Ok(self.files.contains(p) || self.dirs.contains(p) || self.links.contains_key(p))
        }

        fn is_dir(&self, path: &CanonicalHostPath) -> Result<bool, FsNormalizeError> {
            let p = path.as_str();
            if self.links.contains_key(p) {
                return Ok(false);
            }
            if self.dirs.contains(p) || p == "/" || is_drive_root(p) {
                return Ok(true);
            }
            let prefix = format!("{p}/");
            Ok(self.files.iter().any(|f| f.starts_with(&prefix))
                || self.dirs.iter().any(|d| d.starts_with(&prefix))
                || self.links.keys().any(|l| l.starts_with(&prefix)))
        }

        fn read_link(&self, path: &CanonicalHostPath) -> Result<Option<String>, FsNormalizeError> {
            Ok(self.links.get(path.as_str()).cloned())
        }
    }

    fn fixture() -> LexicalFs {
        LexicalFs::new("/repo", "/home/user")
            .with_dir("/repo/src")
            .with_file("/repo/src/lib.rs")
            .with_file("/repo/src/main.rs")
            .with_file("/repo/.gitignore")
            .with_dir("/repo/.git")
            .with_file("/repo/.git/config")
            .with_file("/etc/passwd")
            .with_dir("/tmp")
            .with_file("/tmp/out.txt")
            .with_link("/repo/src/alias", "/repo/src/lib.rs")
            .with_link("/repo/src/chain", "/repo/src/alias")
            .with_link("/repo/src/rel", "lib.rs")
            .with_link("/repo/src/escape", "/etc/passwd")
            .with_link("/repo/src/up", "../.gitignore")
            .with_link("/repo/src/out", "../../etc/passwd")
            .with_link("/repo/src/loop_a", "/repo/src/loop_b")
            .with_link("/repo/src/loop_b", "/repo/src/loop_a")
            .with_link("/repo/src/junc", "C:/Windows/System32")
            .with_link("/repo/src/uncjunc", r"\\?\C:\secret")
            .with_link("/repo/src/in_junc", "/repo/src/lib.rs")
            .with_dangling_link("/repo/src/dangle", "/etc/passwd")
            .with_dangling_link("/repo/src/dangle_rel", "../../etc/passwd")
            .with_dangling_link("/repo/src/dangle_missing", "/tmp/outside-secret")
            .with_dir("/home/user")
            .with_file("/home/user/notes.txt")
    }

    fn normalize(intent: FsIntent) -> CanonicalFsAction {
        normalize_fs(&intent, &fixture(), &CancellationToken::new()).expect("normalize")
    }

    fn reject(intent: FsIntent) -> FsNormalizeError {
        normalize_fs(&intent, &fixture(), &CancellationToken::new()).expect_err("reject")
    }

    #[test]
    fn equivalent_repo_paths_normalize_identically() {
        let a = normalize(FsIntent::read(FilesystemRoot::Repo, "src/lib.rs"));
        let b = normalize(FsIntent::read(FilesystemRoot::Repo, "./src/./lib.rs"));
        let c = normalize(FsIntent::read(FilesystemRoot::Repo, r"src\lib.rs"));
        let d = normalize(FsIntent::read(FilesystemRoot::Repo, "src/alias"));
        let e = normalize(FsIntent::read(FilesystemRoot::Repo, "src/chain"));
        let f = normalize(FsIntent::read(FilesystemRoot::Repo, "src/rel"));
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.path().as_str(), "src/lib.rs");
        assert_eq!(a.root(), FilesystemRoot::Repo);
        assert_eq!(a.operation(), FsOpKind::Read);
        assert!(a.path().existed());
        assert_eq!(d.path().as_str(), "src/lib.rs");
        assert_eq!(e.path().as_str(), "src/lib.rs");
        assert_eq!(f.path().as_str(), "src/lib.rs");
        assert_eq!(a.policy_bytes(), b.policy_bytes());
        assert_eq!(a.policy_bytes(), d.policy_bytes());
    }

    #[test]
    fn traversal_cannot_produce_in_scope_canonical_action() {
        for sample in [
            "../src/lib.rs",
            "src/../src/lib.rs",
            "src/../../etc/passwd",
            "..",
            "src/lib.rs/../../../etc/passwd",
            r"src\..\lib.rs",
        ] {
            let err = reject(FsIntent::read(FilesystemRoot::Repo, sample));
            assert_eq!(err, FsNormalizeError::Traversal, "{sample}");
            assert!(!err.to_string().contains(sample));
            assert!(!err.to_string().contains(".."));
        }
        let host_err = reject(FsIntent::read(FilesystemRoot::Host, "/tmp/../etc/passwd"));
        assert_eq!(host_err, FsNormalizeError::Traversal);
    }

    #[test]
    fn symlink_escape_cannot_produce_repo_identity() {
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/escape")),
            FsNormalizeError::Escape
        );
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Repo, "src/escape")),
            FsNormalizeError::Escape
        );
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/out")),
            FsNormalizeError::Escape
        );
        assert!(!FsNormalizeError::Escape.to_string().contains("/etc/passwd"));
    }

    #[test]
    fn relative_symlink_staying_in_repo_resolves() {
        let action = normalize(FsIntent::read(FilesystemRoot::Repo, "src/up"));
        assert_eq!(action.path().as_str(), ".gitignore");
        assert_eq!(action.root(), FilesystemRoot::Repo);
    }

    #[test]
    fn missing_final_path_allowed_for_write_and_create() {
        let write = normalize(FsIntent::write(FilesystemRoot::Repo, "src/new.rs"));
        assert_eq!(write.path().as_str(), "src/new.rs");
        assert!(!write.path().existed());
        assert_eq!(write.operation(), FsOpKind::Write);
        let create = normalize(FsIntent::create(FilesystemRoot::Repo, "src/brand.rs"));
        assert_eq!(create.path().as_str(), "src/brand.rs");
        assert!(!create.path().existed());
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/new.rs")),
            FsNormalizeError::UnresolvedPath
        );
        assert_eq!(
            reject(FsIntent::delete(FilesystemRoot::Repo, "src/new.rs")),
            FsNormalizeError::UnresolvedPath
        );
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Repo, "missing/new.rs")),
            FsNormalizeError::UnresolvedParent
        );
    }

    #[test]
    fn write_through_symlink_binds_final_identity() {
        let action = normalize(FsIntent::write(FilesystemRoot::Repo, "src/alias"));
        assert_eq!(action.path().as_str(), "src/lib.rs");
        assert!(action.path().existed());
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Repo, "src/escape")),
            FsNormalizeError::Escape
        );
    }

    #[test]
    fn rename_and_delete_use_link_identity_not_target() {
        let delete = normalize(FsIntent::delete(FilesystemRoot::Repo, "src/alias"));
        assert_eq!(delete.path().as_str(), "src/alias");
        assert_eq!(delete.operation(), FsOpKind::Delete);
        assert!(delete.dest().is_none());

        let rename = normalize(FsIntent::rename(
            FilesystemRoot::Repo,
            "src/alias",
            "src/moved.rs",
        ));
        assert_eq!(rename.operation(), FsOpKind::Rename);
        assert_eq!(rename.path().as_str(), "src/alias");
        assert!(rename.path().existed());
        let dest = rename.dest().expect("dest");
        assert_eq!(dest.as_str(), "src/moved.rs");
        assert!(!dest.existed());

        assert_eq!(
            reject(FsIntent::rename(
                FilesystemRoot::Repo,
                "src/missing.rs",
                "src/other.rs",
            )),
            FsNormalizeError::UnresolvedPath
        );
    }

    #[test]
    fn windows_junction_like_escape_is_rejected() {
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/junc")),
            FsNormalizeError::Escape
        );
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Repo, "src/junc")),
            FsNormalizeError::Escape
        );
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/uncjunc")),
            FsNormalizeError::Unc
        );
        let in_repo = normalize(FsIntent::read(FilesystemRoot::Repo, "src/in_junc"));
        assert_eq!(in_repo.path().as_str(), "src/lib.rs");
    }

    #[test]
    fn repo_rejects_absolute_drive_and_unc() {
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "/etc/passwd")),
            FsNormalizeError::AbsolutePath
        );
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, r"C:\Windows")),
            FsNormalizeError::WindowsDrive
        );
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, r"\\server\share")),
            FsNormalizeError::Unc
        );
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Host, r"\\server\share\file")),
            FsNormalizeError::Unc
        );
    }

    #[test]
    fn host_and_repo_identities_stay_distinct() {
        let repo = normalize(FsIntent::read(FilesystemRoot::Repo, "src/lib.rs"));
        let host = normalize(FsIntent::read(FilesystemRoot::Host, "/repo/src/lib.rs"));
        assert_eq!(repo.root(), FilesystemRoot::Repo);
        assert_eq!(host.root(), FilesystemRoot::Host);
        assert_eq!(repo.path().as_str(), "src/lib.rs");
        assert_eq!(host.path().as_str(), "/repo/src/lib.rs");
        assert_ne!(repo.policy_bytes(), host.policy_bytes());
        let rel_host = normalize(FsIntent::read(FilesystemRoot::Host, "notes.txt"));
        assert_eq!(rel_host.path().as_str(), "/home/user/notes.txt");
    }

    #[test]
    fn dangling_repo_symlink_to_out_of_scope_cannot_produce_in_scope_action() {
        for sample in ["src/dangle", "src/dangle_rel", "src/dangle_missing"] {
            for intent in [
                FsIntent::write(FilesystemRoot::Repo, sample),
                FsIntent::create(FilesystemRoot::Repo, sample),
                FsIntent::read(FilesystemRoot::Repo, sample),
            ] {
                let err = reject(intent);
                assert!(
                    matches!(
                        err,
                        FsNormalizeError::Escape | FsNormalizeError::UnresolvedPath
                    ),
                    "{sample}: {err:?}"
                );
                assert!(!err.to_string().contains(sample));
                assert!(!err.to_string().contains("/etc/passwd"));
                assert!(!err.to_string().contains("outside-secret"));
            }
        }
    }

    #[test]
    fn git_directory_mutation_is_not_generic_fs_write() {
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Repo, ".git/config")),
            FsNormalizeError::GitScopeRequired
        );
        assert_eq!(
            reject(FsIntent::create(FilesystemRoot::Repo, ".git/new-file")),
            FsNormalizeError::GitScopeRequired
        );
        assert_eq!(
            reject(FsIntent::delete(FilesystemRoot::Repo, ".git/config")),
            FsNormalizeError::GitScopeRequired
        );
        assert_eq!(
            reject(FsIntent::rename(
                FilesystemRoot::Repo,
                "src/lib.rs",
                ".git/evil",
            )),
            FsNormalizeError::GitScopeRequired
        );
        let read = normalize(FsIntent::read(FilesystemRoot::Repo, ".git/config"));
        assert_eq!(read.path().as_str(), ".git/config");
        let ignore = normalize(FsIntent::write(FilesystemRoot::Repo, ".gitignore"));
        assert_eq!(ignore.path().as_str(), ".gitignore");
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Host, "/repo/.git/config")),
            FsNormalizeError::GitScopeRequired
        );
    }

    #[test]
    fn ascii_case_insensitive_git_components_require_git_scope() {
        for sample in [".GIT/config", ".Git/HEAD", ".gIt/objects"] {
            assert_eq!(
                reject(FsIntent::write(FilesystemRoot::Repo, sample)),
                FsNormalizeError::GitScopeRequired,
                "{sample}"
            );
            assert_eq!(
                reject(FsIntent::create(FilesystemRoot::Repo, sample)),
                FsNormalizeError::GitScopeRequired,
                "{sample}"
            );
            assert_eq!(
                reject(FsIntent::delete(FilesystemRoot::Repo, sample)),
                FsNormalizeError::GitScopeRequired,
                "{sample}"
            );
        }
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Host, "/repo/.GIT/config")),
            FsNormalizeError::GitScopeRequired
        );
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Host, "/repo/.Git/HEAD")),
            FsNormalizeError::GitScopeRequired
        );
        assert_eq!(
            reject(FsIntent::rename(
                FilesystemRoot::Repo,
                "src/lib.rs",
                ".GIT/evil",
            )),
            FsNormalizeError::GitScopeRequired
        );
        let ignore = normalize(FsIntent::write(FilesystemRoot::Repo, ".gitignore"));
        assert_eq!(ignore.path().as_str(), ".gitignore");
        let github = normalize(FsIntent::write(FilesystemRoot::Repo, ".github"));
        assert_eq!(github.path().as_str(), ".github");
        assert!(!github.path().existed());
        assert_ne!(
            FsNormalizeError::GitScopeRequired.to_string(),
            FsNormalizeError::Escape.to_string()
        );
        assert!(
            !FsNormalizeError::GitScopeRequired
                .to_string()
                .contains(".GIT")
        );
    }

    #[test]
    fn symlink_loop_and_cancel_fail_closed() {
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/loop_a")),
            FsNormalizeError::SymlinkLoop
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = normalize_fs(
            &FsIntent::read(FilesystemRoot::Repo, "src/lib.rs"),
            &fixture(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, FsNormalizeError::Cancelled);
    }

    #[test]
    fn nul_control_bounds_and_empty_fail_closed() {
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "")),
            FsNormalizeError::EmptyPath
        );
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/\0lib.rs")),
            FsNormalizeError::Nul
        );
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, "src/\nlib.rs")),
            FsNormalizeError::Control
        );
        let too_long = "a".repeat(MAX_FS_PATH_BYTES + 1);
        assert_eq!(
            reject(FsIntent::read(FilesystemRoot::Repo, too_long)),
            FsNormalizeError::TooLong
        );
        assert_eq!(
            reject(FsIntent::rename(FilesystemRoot::Repo, "src/lib.rs", "")),
            FsNormalizeError::EmptyPath
        );
        assert_eq!(
            reject(FsIntent::write(FilesystemRoot::Repo, "src/lib.rs/extra")),
            FsNormalizeError::NotADirectory
        );
    }

    #[test]
    fn error_display_does_not_echo_input() {
        let err = reject(FsIntent::read(FilesystemRoot::Repo, "../secret"));
        let shown = err.to_string();
        assert_eq!(shown, "filesystem path contains a traversal component");
        assert!(!shown.contains("secret"));
        assert!(!shown.contains(".."));
    }

    #[test]
    fn policy_bytes_are_stable() {
        let read = normalize(FsIntent::read(FilesystemRoot::Repo, "src/lib.rs"));
        let expected: &[u8] = b"rapidlm.canonical_fs.v1\0repo\0read\0src/lib.rs\0";
        assert_eq!(read.policy_bytes(), expected);
        let rename = normalize(FsIntent::rename(
            FilesystemRoot::Repo,
            "src/lib.rs",
            "src/moved.rs",
        ));
        assert_eq!(
            rename.policy_bytes(),
            b"rapidlm.canonical_fs.v1\0repo\0rename\0src/lib.rs\0src/moved.rs\0"
        );
        assert_ne!(read.policy_bytes(), rename.policy_bytes());
    }
}
