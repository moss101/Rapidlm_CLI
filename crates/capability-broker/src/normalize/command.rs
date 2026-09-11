//! Command action normalizer.
//!
//! Resolves executable and cwd, then freezes argv, env *names*, and shell mode
//! before any policy hash. Shell-string mode is never inferred from argv.
//! Env values are not accepted and cannot appear in the canonical form.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum UTF-8 bytes for a requested or resolved path.
pub const MAX_PATH_BYTES: usize = 4096;

/// Maximum UTF-8 bytes for one argv token.
pub const MAX_ARG_BYTES: usize = 4096;

/// Maximum argv tokens (including argv0).
pub const MAX_ARGV: usize = 256;

/// Maximum UTF-8 bytes for one environment variable name.
pub const MAX_ENV_NAME_BYTES: usize = 256;

/// Maximum distinct environment variable names.
pub const MAX_ENV_NAMES: usize = 256;

/// Maximum UTF-8 bytes for an explicit shell script.
pub const MAX_SHELL_SCRIPT_BYTES: usize = 64 * 1024;

const POLICY_TAG: &[u8] = b"rapidlm.canonical_command.v1";
const CANCEL_STRIDE: usize = 16;

/// Cooperative cancellation for normalization loops.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Untrusted execution request. Model/tool supplied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecIntent {
    invocation: CommandInvocation,
    cwd: String,
    env_names: Vec<String>,
}

/// How the command was requested. The two arms are not interchangeable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandInvocation {
    /// Direct exec. `argv[0]` is the requested executable.
    Argv { argv: Vec<String> },
    /// Explicit shell-string mode. Distinct from argv even when argv looks like `sh -c …`.
    Shell { shell: String, script: String },
}

/// Frozen invocation class used in the action hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ShellMode {
    Argv,
    ShellString,
}

/// Host-canonical path after resolver identity (absolute, `/` separators).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CanonicalHostPath(String);

/// Normalized command bound into policy hashing.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CanonicalCommand {
    executable: CanonicalHostPath,
    argv: Vec<String>,
    cwd: CanonicalHostPath,
    env_names: Vec<String>,
    mode: ShellMode,
    shell_script: Option<String>,
}

/// Resolves requested executable and cwd into host-canonical identities.
///
/// Search `PATH` must come from this trusted resolver, never from intent env names.
pub trait Resolver {
    fn resolve_cwd(&self, requested: &str) -> Result<CanonicalHostPath, CommandNormalizeError>;

    fn resolve_executable(
        &self,
        requested: &str,
        cwd: &CanonicalHostPath,
    ) -> Result<CanonicalHostPath, CommandNormalizeError>;
}

/// Resolves `cwd`/executable via real filesystem syscalls (`std::fs::
/// canonicalize`, which follows symlinks through the OS) rather than pure
/// lexical `..`/`.`-folding, so a symlink retargeted between lease approval
/// and use resolves to a *different* path and fails the lease's action-hash
/// comparison — the same real-syscall verification `normalize::fs` already
/// does for filesystem writes (`FsResolver::read_link`, walked component by
/// component), applied here to commands for the first time. Every
/// production caller that mints or re-verifies a `proc.exec` lease should
/// use this rather than a bespoke lexical-only resolver, and — critically —
/// the *same* one on both the issuing and the re-verifying side: comparing
/// a real resolution against a lexical one would disagree on any symlinked
/// executable even with no attack involved, not just a swapped one.
///
/// Callers are expected to have already resolved `requested` to an absolute
/// path (`$PATH` search, workspace-root-relative resolution, etc. — this
/// resolver does not search `$PATH` itself, matching every resolver it
/// replaces) — this only verifies what that path currently, really points
/// at. Does not defend against a plain regular file's *content* being
/// swapped in place with no symlink involved (that needs a fingerprint
/// bound into the canonical form itself, a larger data-model change, not
/// attempted here) — this closes the symlink-retargeting class of attack
/// specifically, at parity with what `normalize::fs` already guarantees for
/// writes, not a strictly stronger guarantee than that established
/// precedent.
pub struct LiveHostResolver;

impl LiveHostResolver {
    fn canonicalize(requested: &str) -> Result<String, ()> {
        let canonical = std::fs::canonicalize(requested).map_err(|_| ())?;
        let text = canonical.to_str().ok_or(())?;
        // `std::fs::canonicalize` on Windows returns a `\\?\`-prefixed
        // (verbatim) path; `CanonicalHostPath::from_resolved` rejects UNC
        // paths outright, which would otherwise make every real resolution
        // fail closed on that platform. Documented Rust stdlib behavior,
        // not a workaround for anything unusual — strip the prefix, which
        // still names the identical real path, just in the same non-
        // verbatim form every other caller already produces.
        let stripped = text.strip_prefix(r"\\?\").unwrap_or(text);
        Ok(stripped.to_owned())
    }
}

impl Resolver for LiveHostResolver {
    fn resolve_cwd(&self, requested: &str) -> Result<CanonicalHostPath, CommandNormalizeError> {
        let resolved =
            Self::canonicalize(requested).map_err(|()| CommandNormalizeError::UnresolvedCwd)?;
        CanonicalHostPath::from_resolved(&resolved)
    }

    fn resolve_executable(
        &self,
        requested: &str,
        _cwd: &CanonicalHostPath,
    ) -> Result<CanonicalHostPath, CommandNormalizeError> {
        let resolved = Self::canonicalize(requested)
            .map_err(|()| CommandNormalizeError::UnresolvedExecutable)?;
        CanonicalHostPath::from_resolved(&resolved)
            .map_err(|_| CommandNormalizeError::UnresolvedExecutable)
    }
}

/// Typed normalize failure. Display never echoes attacker-controlled input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandNormalizeError {
    Cancelled,
    EmptyExecutable,
    EmptyArgv,
    EmptyCwd,
    EmptyEnvName,
    EmptyShell,
    EmptyScript,
    TooLong,
    TooManyArgs,
    TooManyEnvNames,
    Nul,
    Control,
    Unc,
    Traversal,
    InvalidEnvName,
    UnresolvedExecutable,
    UnresolvedCwd,
    SymlinkLoop,
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

    pub fn check(&self) -> Result<(), CommandNormalizeError> {
        if self.is_cancelled() {
            Err(CommandNormalizeError::Cancelled)
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

impl ExecIntent {
    pub fn argv(
        argv: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<String>,
        env_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            invocation: CommandInvocation::Argv {
                argv: argv.into_iter().map(Into::into).collect(),
            },
            cwd: cwd.into(),
            env_names: env_names.into_iter().map(Into::into).collect(),
        }
    }

    pub fn shell(
        shell: impl Into<String>,
        script: impl Into<String>,
        cwd: impl Into<String>,
        env_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            invocation: CommandInvocation::Shell {
                shell: shell.into(),
                script: script.into(),
            },
            cwd: cwd.into(),
            env_names: env_names.into_iter().map(Into::into).collect(),
        }
    }

    pub fn invocation(&self) -> &CommandInvocation {
        &self.invocation
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn env_names(&self) -> &[String] {
        &self.env_names
    }
}

impl ShellMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Argv => "argv",
            Self::ShellString => "shell",
        }
    }
}

impl CanonicalHostPath {
    /// Accept a resolver-produced absolute identity. Leftover `..` fails closed.
    pub fn from_resolved(path: &str) -> Result<Self, CommandNormalizeError> {
        validate_text(path, MAX_PATH_BYTES, ControlPolicy::RejectAll)?;
        if path.is_empty() {
            return Err(CommandNormalizeError::EmptyCwd);
        }
        if is_unc(path) {
            return Err(CommandNormalizeError::Unc);
        }
        let (prefix, rest) = split_drive(path);
        if prefix.is_none() && !path.starts_with('/') && !path.starts_with('\\') {
            return Err(CommandNormalizeError::UnresolvedCwd);
        }
        let mut parts = Vec::new();
        for component in rest.split(['/', '\\']) {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                return Err(CommandNormalizeError::Traversal);
            }
            parts.push(component);
        }
        let mut normalized = match prefix {
            Some(drive) => {
                let mut out = String::from(drive);
                out.push('/');
                out.push_str(&parts.join("/"));
                out
            }
            None => {
                let mut out = String::from("/");
                out.push_str(&parts.join("/"));
                if parts.is_empty() {
                    out = String::from("/");
                }
                out
            }
        };
        if normalized.ends_with('/') && normalized.len() > 1 && !is_drive_root(&normalized) {
            normalized.pop();
        }
        if normalized.len() > MAX_PATH_BYTES {
            return Err(CommandNormalizeError::TooLong);
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CanonicalHostPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CanonicalHostPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl CanonicalCommand {
    pub fn executable(&self) -> &CanonicalHostPath {
        &self.executable
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn cwd(&self) -> &CanonicalHostPath {
        &self.cwd
    }

    pub fn env_names(&self) -> &[String] {
        &self.env_names
    }

    pub fn mode(&self) -> ShellMode {
        self.mode
    }

    pub fn shell_script(&self) -> Option<&str> {
        self.shell_script.as_deref()
    }

    /// Stable bytes for action-hash input. Env values are never included.
    pub fn policy_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.argv.iter().map(String::len).sum::<usize>());
        out.extend_from_slice(POLICY_TAG);
        out.push(0);
        out.extend_from_slice(self.mode.as_str().as_bytes());
        out.push(0);
        out.extend_from_slice(self.executable.as_str().as_bytes());
        out.push(0);
        out.extend_from_slice(self.cwd.as_str().as_bytes());
        out.push(0);
        match self.mode {
            ShellMode::Argv => {
                out.extend_from_slice(b"argv");
                out.push(0);
                for arg in &self.argv {
                    out.extend_from_slice(arg.as_bytes());
                    out.push(0);
                }
            }
            ShellMode::ShellString => {
                out.extend_from_slice(b"script");
                out.push(0);
                if let Some(script) = &self.shell_script {
                    out.extend_from_slice(script.as_bytes());
                }
                out.push(0);
            }
        }
        out.extend_from_slice(b"env");
        out.push(0);
        for name in &self.env_names {
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        out
    }
}

/// Normalize executable, argv, cwd, env names, and shell mode before hashing.
pub fn normalize_exec<R: Resolver + ?Sized>(
    intent: &ExecIntent,
    resolver: &R,
    cancel: &CancellationToken,
) -> Result<CanonicalCommand, CommandNormalizeError> {
    cancel.check()?;
    let cwd_req = validate_requested_path(&intent.cwd, CommandNormalizeError::EmptyCwd)?;
    let cwd = resolver.resolve_cwd(cwd_req)?;
    cancel.check()?;

    let env_names = normalize_env_names(&intent.env_names, cancel)?;

    match &intent.invocation {
        CommandInvocation::Argv { argv } => {
            if argv.is_empty() {
                return Err(CommandNormalizeError::EmptyArgv);
            }
            if argv.len() > MAX_ARGV {
                return Err(CommandNormalizeError::TooManyArgs);
            }
            let requested_exe = argv[0].as_str();
            if requested_exe.is_empty() {
                return Err(CommandNormalizeError::EmptyExecutable);
            }
            let executable = resolve_validated_executable(requested_exe, &cwd, resolver)?;
            let mut normalized_argv = Vec::with_capacity(argv.len());
            normalized_argv.push(executable.as_str().to_owned());
            for (i, arg) in argv.iter().enumerate().skip(1) {
                if i % CANCEL_STRIDE == 0 {
                    cancel.check()?;
                }
                validate_argv_token(arg)?;
                normalized_argv.push(arg.clone());
            }
            Ok(CanonicalCommand {
                executable,
                argv: normalized_argv,
                cwd,
                env_names,
                mode: ShellMode::Argv,
                shell_script: None,
            })
        }
        CommandInvocation::Shell { shell, script } => {
            if shell.is_empty() {
                return Err(CommandNormalizeError::EmptyShell);
            }
            if script.is_empty() {
                return Err(CommandNormalizeError::EmptyScript);
            }
            validate_text(shell, MAX_PATH_BYTES, ControlPolicy::RejectAll)?;
            validate_text(script, MAX_SHELL_SCRIPT_BYTES, ControlPolicy::AllowShell)?;
            if is_unc(shell) {
                return Err(CommandNormalizeError::Unc);
            }
            let executable = resolver.resolve_executable(shell, &cwd)?;
            Ok(CanonicalCommand {
                argv: vec![executable.as_str().to_owned()],
                executable,
                cwd,
                env_names,
                mode: ShellMode::ShellString,
                shell_script: Some(script.clone()),
            })
        }
    }
}

fn resolve_validated_executable<R: Resolver + ?Sized>(
    requested: &str,
    cwd: &CanonicalHostPath,
    resolver: &R,
) -> Result<CanonicalHostPath, CommandNormalizeError> {
    validate_text(requested, MAX_PATH_BYTES, ControlPolicy::RejectAll)?;
    if is_unc(requested) {
        return Err(CommandNormalizeError::Unc);
    }
    resolver.resolve_executable(requested, cwd)
}

fn normalize_env_names(
    names: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<String>, CommandNormalizeError> {
    if names.len() > MAX_ENV_NAMES {
        return Err(CommandNormalizeError::TooManyEnvNames);
    }
    let mut set = BTreeSet::new();
    for (i, name) in names.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            cancel.check()?;
        }
        validate_env_name(name)?;
        set.insert(name.clone());
    }
    if set.len() > MAX_ENV_NAMES {
        return Err(CommandNormalizeError::TooManyEnvNames);
    }
    Ok(set.into_iter().collect())
}

fn validate_env_name(name: &str) -> Result<(), CommandNormalizeError> {
    if name.is_empty() {
        return Err(CommandNormalizeError::EmptyEnvName);
    }
    if name.len() > MAX_ENV_NAME_BYTES {
        return Err(CommandNormalizeError::TooLong);
    }
    if name.contains('\0') {
        return Err(CommandNormalizeError::Nul);
    }
    if name.chars().any(char::is_control) {
        return Err(CommandNormalizeError::Control);
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(CommandNormalizeError::InvalidEnvName);
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(CommandNormalizeError::InvalidEnvName);
    }
    Ok(())
}

fn validate_argv_token(arg: &str) -> Result<(), CommandNormalizeError> {
    validate_text(arg, MAX_ARG_BYTES, ControlPolicy::RejectAll)
}

fn validate_requested_path(
    path: &str,
    empty: CommandNormalizeError,
) -> Result<&str, CommandNormalizeError> {
    if path.is_empty() {
        return Err(empty);
    }
    validate_text(path, MAX_PATH_BYTES, ControlPolicy::RejectAll)?;
    if is_unc(path) {
        return Err(CommandNormalizeError::Unc);
    }
    Ok(path)
}

#[derive(Clone, Copy)]
enum ControlPolicy {
    RejectAll,
    AllowShell,
}

fn validate_text(
    value: &str,
    max_bytes: usize,
    controls: ControlPolicy,
) -> Result<(), CommandNormalizeError> {
    if value.len() > max_bytes {
        return Err(CommandNormalizeError::TooLong);
    }
    if value.contains('\0') {
        return Err(CommandNormalizeError::Nul);
    }
    match controls {
        ControlPolicy::RejectAll => {
            if value.chars().any(char::is_control) {
                return Err(CommandNormalizeError::Control);
            }
        }
        ControlPolicy::AllowShell => {
            if value
                .chars()
                .any(|c| char::is_control(c) && c != '\n' && c != '\t' && c != '\r')
            {
                return Err(CommandNormalizeError::Control);
            }
        }
    }
    Ok(())
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

impl fmt::Display for ShellMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CommandNormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "command normalization cancelled",
            Self::EmptyExecutable => "executable is empty",
            Self::EmptyArgv => "argv is empty",
            Self::EmptyCwd => "cwd is empty",
            Self::EmptyEnvName => "environment variable name is empty",
            Self::EmptyShell => "shell executable is empty",
            Self::EmptyScript => "shell script is empty",
            Self::TooLong => "command field exceeds bound",
            Self::TooManyArgs => "argv exceeds bound",
            Self::TooManyEnvNames => "environment name list exceeds bound",
            Self::Nul => "command field contains NUL",
            Self::Control => "command field contains a control character",
            Self::Unc => "UNC path is not a local command path",
            Self::Traversal => "resolved command path contains a traversal component",
            Self::InvalidEnvName => "invalid environment variable name",
            Self::UnresolvedExecutable => "executable could not be resolved",
            Self::UnresolvedCwd => "cwd could not be resolved",
            Self::SymlinkLoop => "executable or cwd symlink loop",
        })
    }
}

impl Error for CommandNormalizeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    const MAX_SYMLINK_HOPS: usize = 32;

    struct LexicalResolver {
        workspace: String,
        path_dirs: Vec<String>,
        files: BTreeSet<String>,
        links: BTreeMap<String, String>,
    }

    impl LexicalResolver {
        fn new(workspace: &str) -> Self {
            Self {
                workspace: workspace.to_owned(),
                path_dirs: Vec::new(),
                files: BTreeSet::new(),
                links: BTreeMap::new(),
            }
        }

        fn with_file(mut self, path: &str) -> Self {
            self.files.insert(path.to_owned());
            self
        }

        fn with_path_dir(mut self, dir: &str) -> Self {
            self.path_dirs.push(dir.to_owned());
            self
        }

        fn with_link(mut self, from: &str, to: &str) -> Self {
            self.links.insert(from.to_owned(), to.to_owned());
            self
        }

        fn lexical_join(
            &self,
            base: &str,
            requested: &str,
        ) -> Result<String, CommandNormalizeError> {
            if is_unc(requested) {
                return Err(CommandNormalizeError::Unc);
            }
            let absolute = requested.starts_with('/')
                || requested.starts_with('\\')
                || split_drive(requested).0.is_some();
            let joined = if absolute {
                requested.to_owned()
            } else {
                format!("{base}/{requested}")
            };
            lexical_fold(&joined)
        }

        fn follow(&self, start: &str) -> Result<String, CommandNormalizeError> {
            let mut current = start.to_owned();
            for _ in 0..MAX_SYMLINK_HOPS {
                match self.links.get(&current) {
                    Some(target) => {
                        if is_unc(target) {
                            return Err(CommandNormalizeError::Unc);
                        }
                        let next = if target.starts_with('/')
                            || target.starts_with('\\')
                            || split_drive(target).0.is_some()
                        {
                            lexical_fold(target)?
                        } else {
                            let parent = parent_of(&current);
                            lexical_fold(&format!("{parent}/{target}"))?
                        };
                        current = next;
                    }
                    None => return Ok(current),
                }
            }
            Err(CommandNormalizeError::SymlinkLoop)
        }

        fn resolve_abs(
            &self,
            requested: &str,
            base: &str,
        ) -> Result<CanonicalHostPath, CommandNormalizeError> {
            let folded = self.lexical_join(base, requested)?;
            let followed = self.follow(&folded)?;
            CanonicalHostPath::from_resolved(&followed)
        }
    }

    impl Resolver for LexicalResolver {
        fn resolve_cwd(&self, requested: &str) -> Result<CanonicalHostPath, CommandNormalizeError> {
            self.resolve_abs(requested, &self.workspace)
        }

        fn resolve_executable(
            &self,
            requested: &str,
            cwd: &CanonicalHostPath,
        ) -> Result<CanonicalHostPath, CommandNormalizeError> {
            let has_sep = requested.contains('/') || requested.contains('\\');
            if !has_sep {
                for dir in &self.path_dirs {
                    let candidate = self.resolve_abs(requested, dir)?;
                    if self.files.contains(candidate.as_str())
                        || self.links.contains_key(candidate.as_str())
                    {
                        let followed = self.follow(candidate.as_str())?;
                        if self.files.contains(&followed) || self.links.contains_key(&followed) {
                            return CanonicalHostPath::from_resolved(&followed);
                        }
                    }
                }
                return Err(CommandNormalizeError::UnresolvedExecutable);
            }
            let resolved = self.resolve_abs(requested, cwd.as_str())?;
            if self.files.contains(resolved.as_str()) || self.links.contains_key(resolved.as_str())
            {
                let followed = self.follow(resolved.as_str())?;
                return CanonicalHostPath::from_resolved(&followed);
            }
            Err(CommandNormalizeError::UnresolvedExecutable)
        }
    }

    fn lexical_fold(path: &str) -> Result<String, CommandNormalizeError> {
        if is_unc(path) {
            return Err(CommandNormalizeError::Unc);
        }
        let (drive, rest) = split_drive(path);
        let rooted = drive.is_some() || path.starts_with('/') || path.starts_with('\\');
        if !rooted {
            return Err(CommandNormalizeError::UnresolvedCwd);
        }
        let mut parts: Vec<&str> = Vec::new();
        for component in rest.split(['/', '\\']) {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                if parts.pop().is_none() {
                    return Err(CommandNormalizeError::Traversal);
                }
                continue;
            }
            parts.push(component);
        }
        let body = parts.join("/");
        let folded = match drive {
            Some(d) => {
                if body.is_empty() {
                    format!("{d}/")
                } else {
                    format!("{d}/{body}")
                }
            }
            None => format!("/{body}"),
        };
        Ok(folded)
    }

    fn parent_of(path: &str) -> String {
        match path.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => parent.to_owned(),
            Some((_, _)) => "/".to_owned(),
            None => "/".to_owned(),
        }
    }

    fn fixture() -> LexicalResolver {
        LexicalResolver::new("/repo")
            .with_path_dir("/usr/bin")
            .with_file("/usr/bin/git")
            .with_file("/usr/bin/bash")
            .with_file("/usr/bin/echo")
            .with_file("/bin/sh")
            .with_link("/usr/local/bin/git", "/usr/bin/git")
            .with_file("/usr/local/bin/git")
    }

    fn normalize(intent: ExecIntent) -> CanonicalCommand {
        normalize_exec(&intent, &fixture(), &CancellationToken::new()).expect("normalize")
    }

    #[test]
    fn equivalent_paths_normalize_identically() {
        let a = normalize(ExecIntent::argv(["git", "status"], "/repo", ["HOME"]));
        let b = normalize(ExecIntent::argv(
            ["/usr/bin/./git", "status"],
            "/repo/src/..",
            ["HOME"],
        ));
        let c = normalize(ExecIntent::argv(
            ["/usr/bin/../bin/git", "status"],
            "/repo/.",
            ["HOME"],
        ));
        let d = normalize(ExecIntent::argv(
            [r"/usr/bin\git", "status"],
            r"/repo\src\..",
            ["HOME"],
        ));
        let e = normalize(ExecIntent::argv(
            ["/usr/local/bin/git", "status"],
            "/repo",
            ["HOME"],
        ));
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a, d);
        assert_eq!(a, e);
        assert_eq!(a.executable().as_str(), "/usr/bin/git");
        assert_eq!(a.cwd().as_str(), "/repo");
        assert_eq!(a.argv(), &["/usr/bin/git".to_owned(), "status".to_owned()]);
        assert_eq!(a.mode(), ShellMode::Argv);
        assert_eq!(a.shell_script(), None);
        assert_eq!(a.policy_bytes(), b.policy_bytes());
    }

    #[test]
    fn shell_string_mode_is_distinct_from_argv() {
        let argv = normalize(ExecIntent::argv(["echo", "hi"], "/repo", None::<String>));
        let shell = normalize(ExecIntent::shell(
            "bash",
            "echo hi",
            "/repo",
            None::<String>,
        ));
        let sh_c = normalize(ExecIntent::argv(
            ["/bin/sh", "-c", "echo hi"],
            "/repo",
            None::<String>,
        ));
        assert_eq!(argv.mode(), ShellMode::Argv);
        assert_eq!(shell.mode(), ShellMode::ShellString);
        assert_eq!(sh_c.mode(), ShellMode::Argv);
        assert_ne!(argv, shell);
        assert_ne!(shell, sh_c);
        assert_ne!(argv.policy_bytes(), shell.policy_bytes());
        assert_ne!(shell.policy_bytes(), sh_c.policy_bytes());
        assert_eq!(shell.shell_script(), Some("echo hi"));
        assert_eq!(sh_c.shell_script(), None);
        assert_eq!(sh_c.argv(), &["/bin/sh", "-c", "echo hi"]);
        assert_eq!(shell.argv(), &["/usr/bin/bash"]);
    }

    #[test]
    fn env_names_sort_and_dedup_without_values() {
        let a = normalize(ExecIntent::argv(
            ["git", "status"],
            "/repo",
            ["PATH", "HOME", "PATH"],
        ));
        let b = normalize(ExecIntent::argv(
            ["git", "status"],
            "/repo",
            ["HOME", "PATH"],
        ));
        assert_eq!(a.env_names(), &["HOME".to_owned(), "PATH".to_owned()]);
        assert_eq!(a, b);
        let bytes = a.policy_bytes();
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        assert!(text.contains("HOME"));
        assert!(text.contains("PATH"));
        assert!(!text.contains('='));
        assert!(!format!("{a:?}").contains("secret"));
    }

    #[test]
    fn env_assignment_smuggling_fails_closed() {
        // T-002 / T-012: names only; `PATH=/tmp/evil` is not an env name.
        let err = normalize_exec(
            &ExecIntent::argv(["git"], "/repo", ["PATH=/tmp/evil"]),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect_err("assignment");
        assert_eq!(err, CommandNormalizeError::InvalidEnvName);
        assert!(!err.to_string().contains("/tmp/evil"));
        assert!(!err.to_string().contains("PATH"));
    }

    #[test]
    fn unresolved_executable_does_not_fall_back_to_raw_name() {
        let err = normalize_exec(
            &ExecIntent::argv(["not-a-real-tool"], "/repo", None::<String>),
            &fixture(),
            &CancellationToken::new(),
        )
        .expect_err("unresolved");
        assert_eq!(err, CommandNormalizeError::UnresolvedExecutable);
        assert!(!err.to_string().contains("not-a-real-tool"));
    }

    #[test]
    fn unc_and_nul_and_control_fail_closed() {
        let cancel = CancellationToken::new();
        let resolver = fixture();
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(["git"], r"\\server\share", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("unc cwd"),
            CommandNormalizeError::Unc
        );
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(["//server/share/tool"], "/repo", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("unc exe"),
            CommandNormalizeError::Unc
        );
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(["git\0"], "/repo", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("nul exe"),
            CommandNormalizeError::Nul
        );
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(["git", "sta\ntus"], "/repo", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("control arg"),
            CommandNormalizeError::Control
        );
    }

    #[test]
    fn bounds_and_empty_inputs_fail_closed() {
        let cancel = CancellationToken::new();
        let resolver = fixture();
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(None::<String>, "/repo", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("empty argv"),
            CommandNormalizeError::EmptyArgv
        );
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv([""], "/repo", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("empty exe"),
            CommandNormalizeError::EmptyExecutable
        );
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(["git"], "", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("empty cwd"),
            CommandNormalizeError::EmptyCwd
        );
        assert_eq!(
            normalize_exec(
                &ExecIntent::shell("bash", "", "/repo", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("empty script"),
            CommandNormalizeError::EmptyScript
        );
        let too_many: Vec<String> = (0..=MAX_ARGV).map(|i| format!("a{i}")).collect();
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(too_many, "/repo", None::<String>),
                &resolver,
                &cancel,
            )
            .expect_err("argv bound"),
            CommandNormalizeError::TooManyArgs
        );
        let too_many_env: Vec<String> = (0..=MAX_ENV_NAMES).map(|i| format!("E{i}")).collect();
        assert_eq!(
            normalize_exec(
                &ExecIntent::argv(["git"], "/repo", too_many_env),
                &resolver,
                &cancel,
            )
            .expect_err("env bound"),
            CommandNormalizeError::TooManyEnvNames
        );
    }

    #[test]
    fn cancelled_normalization_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = normalize_exec(
            &ExecIntent::argv(["git"], "/repo", None::<String>),
            &fixture(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, CommandNormalizeError::Cancelled);
    }

    #[test]
    fn policy_bytes_are_stable_and_omit_env_values() {
        let cmd = normalize(ExecIntent::argv(
            ["git", "status"],
            "/repo",
            ["HOME", "PATH"],
        ));
        let expected: &[u8] = b"rapidlm.canonical_command.v1\0argv\0/usr/bin/git\0/repo\0argv\0/usr/bin/git\0status\0env\0HOME\0PATH\0";
        assert_eq!(cmd.policy_bytes(), expected);
        let shell = normalize(ExecIntent::shell("bash", "echo $SECRET", "/repo", ["HOME"]));
        let shell_bytes = shell.policy_bytes();
        assert_ne!(shell_bytes, expected);
        assert!(shell_bytes.starts_with(b"rapidlm.canonical_command.v1\0shell\0"));
        assert!(!String::from_utf8_lossy(&shell_bytes).contains("SECRET="));
    }

    #[test]
    fn symlink_loop_fails_closed() {
        let resolver = LexicalResolver::new("/repo")
            .with_link("/usr/bin/loop", "/usr/bin/loop")
            .with_file("/usr/bin/loop");
        let err = normalize_exec(
            &ExecIntent::argv(["/usr/bin/loop"], "/repo", None::<String>),
            &resolver,
            &CancellationToken::new(),
        )
        .expect_err("loop");
        assert_eq!(err, CommandNormalizeError::SymlinkLoop);
    }

    #[test]
    fn intent_env_cannot_redirect_path_lookup() {
        // T-002: resolver PATH is trusted; listing PATH in env names must not
        // change which executable identity is hashed.
        let resolver = LexicalResolver::new("/repo")
            .with_path_dir("/usr/bin")
            .with_file("/usr/bin/git")
            .with_file("/tmp/evil/git");
        let honest = normalize_exec(
            &ExecIntent::argv(["git", "status"], "/repo", None::<String>),
            &resolver,
            &CancellationToken::new(),
        )
        .expect("honest");
        let with_name = normalize_exec(
            &ExecIntent::argv(["git", "status"], "/repo", ["PATH"]),
            &resolver,
            &CancellationToken::new(),
        )
        .expect("named path");
        assert_eq!(honest.executable().as_str(), "/usr/bin/git");
        assert_eq!(with_name.executable().as_str(), "/usr/bin/git");
        assert_ne!(with_name.executable().as_str(), "/tmp/evil/git");
    }

    #[cfg(unix)]
    struct TempDir(std::path::PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rapidlm-live-host-resolver-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.subsec_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `LiveHostResolver` is the real-syscall analogue of the `LexicalResolver`
    /// fixture above: this uses an actual temp directory and a real symlink,
    /// not a simulated one, since the whole point is exercising
    /// `std::fs::canonicalize`'s genuine OS-level symlink following.
    #[cfg(unix)]
    #[test]
    fn live_host_resolver_follows_a_real_symlink_to_its_current_target() {
        let dir = TempDir::new("basic");
        let real_target = dir.0.join("real-tool");
        std::fs::write(&real_target, b"#!/bin/sh\n").expect("seed executable");
        let link = dir.0.join("tool-link");
        std::os::unix::fs::symlink(&real_target, &link).expect("symlink");

        let resolved = LiveHostResolver
            .resolve_executable(
                link.to_str().expect("utf8 path"),
                &CanonicalHostPath::from_resolved(dir.0.to_str().expect("utf8 path")).expect("cwd"),
            )
            .expect("resolve through symlink");
        let canonical_real = std::fs::canonicalize(&real_target).expect("canonicalize real target");
        assert_eq!(resolved.as_str(), canonical_real.to_str().expect("utf8"));
    }

    /// The exact TOCTOU scenario this resolver exists to close: resolve the
    /// symlink once (as if at lease-approval time), retarget it, then resolve
    /// again (as if at spawn time) — the two resolutions must disagree, which
    /// is what makes the lease's action-hash comparison reject the swap.
    #[cfg(unix)]
    #[test]
    fn live_host_resolver_disagrees_after_a_symlink_is_retargeted() {
        let dir = TempDir::new("toctou");
        let approved_target = dir.0.join("approved-tool");
        let attacker_target = dir.0.join("attacker-tool");
        std::fs::write(&approved_target, b"#!/bin/sh\necho approved\n").expect("seed approved");
        std::fs::write(&attacker_target, b"#!/bin/sh\necho pwned\n").expect("seed attacker");
        let link = dir.0.join("tool-link");
        std::os::unix::fs::symlink(&approved_target, &link).expect("symlink to approved");

        let cwd =
            CanonicalHostPath::from_resolved(dir.0.to_str().expect("utf8 path")).expect("cwd");
        let approved_resolution = LiveHostResolver
            .resolve_executable(link.to_str().expect("utf8 path"), &cwd)
            .expect("resolve at approval time");

        std::fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&attacker_target, &link).expect("retarget symlink");

        let spawn_time_resolution = LiveHostResolver
            .resolve_executable(link.to_str().expect("utf8 path"), &cwd)
            .expect("resolve at spawn time");

        assert_ne!(
            approved_resolution, spawn_time_resolution,
            "a retargeted symlink must resolve to a different path, so the \
             lease's action-hash comparison rejects the swap"
        );
    }
}
