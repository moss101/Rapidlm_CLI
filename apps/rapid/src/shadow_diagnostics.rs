//! Shadow-verified writes: materialize a candidate edit into an isolated
//! Git worktree and run a configured diagnostics command there — verify
//! before the edit is ever shown to the model as applied, not after.
//!
//! Distinct from `hooks.rs`'s `post_tool_use`: that runs against the real,
//! already-committed workspace after a write lands. This runs against an
//! isolated copy *before* the write ever touches the real tree, so a
//! diagnostics failure never reaches disk.
//!
//! Opt-in via `.rapidlm/settings.json`:
//!
//! ```json
//! { "shadow_diagnostics": { "command": ["python3", "-m", "py_compile", "{path}"],
//!                            "globs": ["*.py"] } }
//! ```
//!
//! `{path}` is substituted with the file's workspace-relative path. An empty
//! or absent `globs` matches every write. Single-file writes and Unix multi-file
//! patches are supported; ordinary single-file patches do not use this path.
//!
//! The isolation backend (`workspace::backends::git_worktree`) requires the
//! trusted root to be a Git repository. When it is not, or worktree setup
//! fails for any reason, this fails **open** to a normal direct write with
//! a warning: a broken shadow-diagnostics config must never brick the write
//! tool. This is a quality feature, not a security boundary — unlike trust
//! and permission gating elsewhere in this crate, which fail closed.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static FORCE_UNCERTAIN_TERMINATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn with_uncertain_termination<T>(work: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORCE_UNCERTAIN_TERMINATION.with(|flag| flag.set(self.0));
        }
    }
    let previous = FORCE_UNCERTAIN_TERMINATION.with(|flag| flag.replace(true));
    let _restore = Restore(previous);
    work()
}

fn termination_verdict(result: std::io::Result<()>) -> std::io::Result<()> {
    #[cfg(test)]
    if FORCE_UNCERTAIN_TERMINATION.with(std::cell::Cell::get) {
        result?;
        return Err(std::io::Error::other(
            "injected uncertain process termination",
        ));
    }
    result
}

use workspace::backends::git_worktree::{GitWorktreeOptions, GitWorktreeStore};
use workspace::view::{CancellationToken, CreateView, ViewAccess, ViewRegistry, WorkspaceBackend};

/// Maximum argv elements accepted for the configured command.
pub const MAX_COMMAND_ARGS: usize = 16;
/// Maximum glob patterns accepted.
pub const MAX_GLOBS: usize = 16;
/// Wall-clock budget for worktree setup and the diagnostics command each.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard byte cap on captured diagnostics output.
pub const MAX_DIAGNOSTICS_OUTPUT_BYTES: usize = 2048;

/// Shadow-diagnostics command parsed from project settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShadowDiagnosticsConfig {
    command: Vec<String>,
    globs: Vec<String>,
}

impl ShadowDiagnosticsConfig {
    /// Parse the `shadow_diagnostics` object from a settings document value.
    /// `None` if absent, malformed, or the command is empty — never a
    /// partially-configured, silently-inert state.
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        let object = value.get("shadow_diagnostics")?.as_object()?;
        let command: Vec<String> = object
            .get("command")?
            .as_array()?
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .take(MAX_COMMAND_ARGS)
            .collect();
        if command.is_empty() {
            return None;
        }
        let globs: Vec<String> = object
            .get("globs")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .take(MAX_GLOBS)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self { command, globs })
    }

    /// True if `relative_path` should be shadow-verified: every configured
    /// glob is checked with the caller's matcher (kept generic so this
    /// module does not duplicate `exec_tools::glob_path_match`); an empty
    /// glob list applies to every write.
    pub fn matches(&self, relative_path: &str, glob_match: impl Fn(&str, &str) -> bool) -> bool {
        if self.globs.is_empty() {
            return true;
        }
        self.globs
            .iter()
            .any(|pattern| glob_match(pattern, relative_path))
    }
}

/// Result of attempting to shadow-verify one candidate write.
#[derive(Debug)]
pub enum ShadowVerifyOutcome {
    /// Diagnostics passed in isolation; safe to apply for real.
    Passed { diagnostics_tail: String },
    /// Diagnostics failed in isolation; the real tree was never touched.
    Failed { diagnostics_tail: String },
    /// A diagnostic process could still be running; no publication or
    /// destructive worktree cleanup is safe until it is resolved.
    Uncertain { reason: String },
    /// Could not verify at all (not a Git repo, worktree setup failed, …).
    /// Never silently treated as "passed" — the caller decides how to
    /// report a skip (this pass: fall back to a direct write).
    Skipped { reason: String },
}

/// Materialize `candidate_content` at `relative_path` inside an isolated
/// worktree of the repo at `root`, run the configured command there, and
/// clean the worktree up regardless of outcome. The real tree at `root` is
/// never touched by this function.
pub fn verify_candidate(
    root: &Path,
    relative_path: &str,
    candidate_content: &[u8],
    config: &ShadowDiagnosticsConfig,
) -> ShadowVerifyOutcome {
    let cancel = CancellationToken::new();
    let store = match GitWorktreeStore::open_with(
        root,
        GitWorktreeOptions::new().with_timeout(DEFAULT_TIMEOUT),
        &cancel,
    ) {
        Ok(store) => store,
        Err(err) => {
            return ShadowVerifyOutcome::Skipped {
                reason: format!("not an isolatable git repository: {err:?}"),
            };
        }
    };
    let spec = CreateView::new(
        protocol::RepoId::new(),
        WorkspaceBackend::GitWorktree,
        "HEAD",
        ViewAccess::ReadWrite,
    )
    .with_write_owner(protocol::AgentId::new());
    let registry = ViewRegistry::new();
    let view = match registry.create(spec, &cancel) {
        Ok(view) => view,
        Err(err) => {
            return ShadowVerifyOutcome::Skipped {
                reason: format!("view create failed: {err:?}"),
            };
        }
    };
    let record = match store.create_view(&view, &cancel) {
        Ok(record) => record,
        Err(err) => {
            return ShadowVerifyOutcome::Skipped {
                reason: format!("worktree create failed: {err:?}"),
            };
        }
    };

    let outcome = if let Some(parent) = record.worktree_path().join(relative_path).parent() {
        if std::fs::create_dir_all(parent).is_err() {
            ShadowVerifyOutcome::Skipped {
                reason: "candidate write failed".to_owned(),
            }
        } else {
            write_and_run(&record, relative_path, candidate_content, config)
        }
    } else {
        write_and_run(&record, relative_path, candidate_content, config)
    };

    // The candidate write (and anything the diagnostics command itself left
    // behind) always dirties the worktree, and `remove_view` deliberately
    // never force-deletes a dirty worktree — that guard exists to protect
    // real work elsewhere, but everything in this worktree was written by
    // this function, so discarding it first is always safe and always
    // correct here. Best-effort: a failed reset/removal is not surfaced as
    // a verify failure (the caller already has a real outcome), only as a
    // warning — worst case a stale worktree accumulates, never a bad edit
    // reaching the real tree.
    if matches!(outcome, ShadowVerifyOutcome::Uncertain { .. }) {
        return outcome;
    }
    discard_worktree_changes(record.worktree_path());
    if let Err(err) = store.remove_view(view.id(), &cancel) {
        eprintln!("warning: shadow-diagnostics worktree cleanup failed: {err:?}");
    }
    outcome
}

/// Materialize all changes into one isolated view before running diagnostics
/// for each selected path. This lets checks observe cross-file changes and
/// deletions as one candidate instead of unrelated single-file snapshots.
#[cfg(unix)]
pub(crate) fn verify_multi_candidate(
    root: &Path,
    candidate: &workspace::multi_edit_recovery::RecoveryRecord,
    config: &ShadowDiagnosticsConfig,
    cancelled: impl Fn() -> bool,
) -> ShadowVerifyOutcome {
    use workspace::multi_edit_recovery::RecoveryState;
    if cancelled() {
        return ShadowVerifyOutcome::Skipped {
            reason: "diagnostics cancelled".into(),
        };
    }
    if candidate.state() != RecoveryState::Intent
        || candidate.has_staging_plan()
        || candidate.encode().is_err()
    {
        return ShadowVerifyOutcome::Skipped {
            reason: "invalid multi-file candidate".into(),
        };
    }
    let selected: Vec<_> = candidate
        .entries()
        .iter()
        .filter(|entry| config.matches(&entry.path, crate::exec_tools::glob_path_match))
        .collect();
    if selected.is_empty() {
        return ShadowVerifyOutcome::Skipped {
            reason: "no paths match the diagnostics configuration".into(),
        };
    }
    let cancel = CancellationToken::new();
    let store = match GitWorktreeStore::open_with(
        root,
        GitWorktreeOptions::new().with_timeout(DEFAULT_TIMEOUT),
        &cancel,
    ) {
        Ok(store) => store,
        Err(err) => {
            return ShadowVerifyOutcome::Skipped {
                reason: format!("not an isolatable git repository: {err:?}"),
            };
        }
    };
    let spec = CreateView::new(
        protocol::RepoId::new(),
        WorkspaceBackend::GitWorktree,
        "HEAD",
        ViewAccess::ReadWrite,
    )
    .with_write_owner(protocol::AgentId::new());
    let registry = ViewRegistry::new();
    let view = match registry.create(spec, &cancel) {
        Ok(view) => view,
        Err(err) => {
            return ShadowVerifyOutcome::Skipped {
                reason: format!("view create failed: {err:?}"),
            };
        }
    };
    let view_record = match store.create_view(&view, &cancel) {
        Ok(record) => record,
        Err(err) => {
            return ShadowVerifyOutcome::Skipped {
                reason: format!("worktree create failed: {err:?}"),
            };
        }
    };
    let worktree = view_record.worktree_path();
    let outcome = if candidate
        .entries()
        .iter()
        .all(|entry| materialize_shadow_entry(worktree, entry).is_ok())
    {
        let mut output = String::new();
        let mut failed = false;
        let mut uncertain = false;
        for entry in selected {
            if cancelled() {
                break;
            }
            let argv: Vec<String> = config
                .command
                .iter()
                .map(|arg| {
                    if arg == "{path}" {
                        entry.path.clone()
                    } else {
                        arg.clone()
                    }
                })
                .collect();
            let (ok, tail, termination_uncertain) =
                run_diagnostics_with_cancel(&argv, worktree, DEFAULT_TIMEOUT, &cancelled);
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("{}: {tail}", entry.path));
            if termination_uncertain {
                uncertain = true;
                break;
            }
            if !candidate
                .entries()
                .iter()
                .all(|entry| shadow_entry_matches(worktree, entry))
            {
                output.push_str("\nshadow diagnostics modified a candidate file");
                failed = true;
                break;
            }
            if !ok {
                failed = true;
                break;
            }
        }
        if uncertain {
            ShadowVerifyOutcome::Uncertain { reason: output }
        } else if cancelled() {
            ShadowVerifyOutcome::Skipped {
                reason: "diagnostics cancelled".into(),
            }
        } else if failed {
            ShadowVerifyOutcome::Failed {
                diagnostics_tail: output,
            }
        } else {
            ShadowVerifyOutcome::Passed {
                diagnostics_tail: output,
            }
        }
    } else {
        ShadowVerifyOutcome::Skipped {
            reason: "multi-file candidate could not be safely materialized".into(),
        }
    };
    if matches!(outcome, ShadowVerifyOutcome::Uncertain { .. }) {
        return outcome;
    }
    discard_worktree_changes(worktree);
    if let Err(err) = store.remove_view(view.id(), &cancel) {
        eprintln!("warning: shadow-diagnostics worktree cleanup failed: {err:?}");
    }
    outcome
}

#[cfg(unix)]
fn materialize_shadow_entry(
    root: &Path,
    entry: &workspace::multi_edit_recovery::RecoveryEntry,
) -> Result<(), ()> {
    let path = Path::new(&entry.path);
    let mut target = root.to_path_buf();
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(part) = component else {
            return Err(());
        };
        target.push(part);
        let parent = components.peek().is_some();
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(()),
            Ok(meta) if parent && !meta.is_dir() => return Err(()),
            Ok(meta) if !parent && !meta.is_file() => return Err(()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && parent => {
                std::fs::create_dir(&target).map_err(|_| ())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }
    match &entry.after {
        Some(image) => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&target, &image.bytes).map_err(|_| ())?;
            if let Some(mode) = image.mode {
                std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))
                    .map_err(|_| ())?;
            }
            Ok(())
        }
        None => match std::fs::remove_file(target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(()),
        },
    }
}

#[cfg(unix)]
fn shadow_entry_matches(
    root: &Path,
    entry: &workspace::multi_edit_recovery::RecoveryEntry,
) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let mut target = root.to_path_buf();
    let mut components = Path::new(&entry.path).components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(part) = component else {
            return false;
        };
        target.push(part);
        let parent = components.peek().is_some();
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => return false,
            Ok(meta) if parent && !meta.is_dir() => return false,
            Ok(meta) if !parent => {
                let Some(image) = &entry.after else {
                    return false;
                };
                if !meta.is_file()
                    || meta.len() != image.bytes.len() as u64
                    || image
                        .mode
                        .is_some_and(|mode| meta.permissions().mode() & 0o7777 != mode)
                {
                    return false;
                }
                return std::fs::read(&target).is_ok_and(|bytes| bytes == image.bytes);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return entry.after.is_none();
            }
            Err(_) => return false,
        }
    }
    false
}

fn discard_worktree_changes(worktree_path: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["checkout", "--quiet", "--", "."])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["clean", "--quiet", "-fd"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn write_and_run(
    record: &workspace::backends::git_worktree::GitWorktreeRecord,
    relative_path: &str,
    candidate_content: &[u8],
    config: &ShadowDiagnosticsConfig,
) -> ShadowVerifyOutcome {
    let target = record.worktree_path().join(relative_path);
    if std::fs::write(&target, candidate_content).is_err() {
        return ShadowVerifyOutcome::Skipped {
            reason: "candidate write failed".to_owned(),
        };
    }
    let argv: Vec<String> = config
        .command
        .iter()
        .map(|arg| {
            if arg == "{path}" {
                relative_path.to_owned()
            } else {
                arg.clone()
            }
        })
        .collect();
    let (ok, output, uncertain) =
        run_diagnostics_with_cancel(&argv, record.worktree_path(), DEFAULT_TIMEOUT, || false);
    if uncertain {
        ShadowVerifyOutcome::Uncertain { reason: output }
    } else if ok {
        ShadowVerifyOutcome::Passed {
            diagnostics_tail: output,
        }
    } else {
        ShadowVerifyOutcome::Failed {
            diagnostics_tail: output,
        }
    }
}

/// Run `argv` with `cwd`, capturing combined stdout+stderr to a temp file
/// (never a pipe: a command that backgrounds children would otherwise hold
/// the pipe open past a kill). Mirrors `hooks::run_hook_once`'s pattern,
/// adapted for an argv command with no shell and no stdin.
#[cfg(test)]
fn run_diagnostics_once(argv: &[String], cwd: &Path, timeout: Duration) -> (bool, String) {
    let (ok, output, _) = run_diagnostics_with_cancel(argv, cwd, timeout, || false);
    (ok, output)
}

fn run_diagnostics_with_cancel(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> (bool, String, bool) {
    let Some((program, args)) = argv.split_first() else {
        return (false, "empty shadow_diagnostics command".to_owned(), false);
    };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let output_path = std::env::temp_dir().join(format!(
        "rapidlm-shadow-diag-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let output_file = match std::fs::File::create(&output_path) {
        Ok(file) => file,
        Err(err) => {
            return (
                false,
                format!("diagnostics output file failed: {err}"),
                false,
            );
        }
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file.try_clone().expect("clone")))
        .stderr(Stdio::from(output_file));
    process_signal::isolate_process_group(&mut command);
    let spawn = command.spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            let _ = std::fs::remove_file(&output_path);
            return (false, format!("diagnostics spawn failed: {err}"), false);
        }
    };
    let started = Instant::now();
    loop {
        match poll_isolated_group(&mut child) {
            Ok(Some(status)) => {
                let output = crate::exec_tools::read_capped_bytes(
                    &output_path,
                    MAX_DIAGNOSTICS_OUTPUT_BYTES,
                );
                let _ = std::fs::remove_file(&output_path);
                return (
                    status.success(),
                    truncate(&output, MAX_DIAGNOSTICS_OUTPUT_BYTES),
                    false,
                );
            }
            Ok(None) => {
                if cancelled() || started.elapsed() > timeout {
                    let termination = terminate_isolated_group(&mut child);
                    let output = crate::exec_tools::read_capped_bytes(
                        &output_path,
                        MAX_DIAGNOSTICS_OUTPUT_BYTES,
                    );
                    let _ = std::fs::remove_file(&output_path);
                    let mut text = truncate(&output, MAX_DIAGNOSTICS_OUTPUT_BYTES);
                    text.push_str(if cancelled() {
                        "\n(diagnostics command cancelled)"
                    } else {
                        "\n(diagnostics command timed out)"
                    });
                    if termination.is_err() {
                        text.push_str("\n(diagnostics process termination is uncertain)");
                    }
                    return (false, text, termination.is_err());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                let termination = terminate_isolated_group(&mut child);
                let _ = std::fs::remove_file(&output_path);
                return (
                    false,
                    format!("diagnostics wait failed: {err}"),
                    termination.is_err(),
                );
            }
        }
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
))]
pub(crate) fn poll_isolated_group(
    child: &mut std::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    match process_signal::child_exited_unreaped(child) {
        Ok(false) => Ok(None),
        Ok(true) => {
            // The unreaped leader reserves its PID until after the group kill.
            process_signal::kill_exited_process_group(child).map_err(std::io::Error::other)?;
            child.wait().map(Some)
        }
        Err(error) => Err(std::io::Error::other(error)),
    }
}

#[cfg(not(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
)))]
pub(crate) fn poll_isolated_group(
    child: &mut std::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    child.try_wait()
}

#[cfg(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
))]
pub(crate) fn terminate_isolated_group(child: &mut std::process::Child) -> std::io::Result<()> {
    // `poll_isolated_group` does not reap a live or exited child, so the
    // process-group ID remains reserved until the wait below completes.
    let group = match process_signal::child_exited_unreaped(child) {
        Ok(true) => process_signal::kill_exited_process_group(child),
        Ok(false) => {
            process_signal::signal_process_group(child.id(), process_signal::GroupSignal::Kill)
        }
        Err(error) => Err(error),
    };
    let _ = child.kill();
    let waited = child.wait();
    group.map_err(std::io::Error::other)?;
    termination_verdict(waited.map(|_| ()))
}

#[cfg(not(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
)))]
pub(crate) fn terminate_isolated_group(child: &mut std::process::Child) -> std::io::Result<()> {
    process_signal::terminate_process_group_default(child);
    termination_verdict(Ok(()))
}

fn truncate(bytes: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= max {
        return text.into_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn seeded_repo(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rapidlm-shadow-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        git(&dir, &["init", "-b", "main"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("seed.txt"), b"seed\n").expect("seed");
        git(&dir, &["add", "seed.txt"]);
        git(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-m",
                "seed",
            ],
        );
        dir
    }

    #[test]
    fn parse_requires_a_nonempty_command() {
        let value =
            serde_json::json!({ "shadow_diagnostics": { "command": [], "globs": ["*.py"] } });
        assert!(ShadowDiagnosticsConfig::parse(&value).is_none());

        let value = serde_json::json!({});
        assert!(ShadowDiagnosticsConfig::parse(&value).is_none());

        let value = serde_json::json!({ "shadow_diagnostics": { "command": ["true"] } });
        let config = ShadowDiagnosticsConfig::parse(&value).expect("parsed");
        assert!(config.matches("anything.py", |_, _| true));
    }

    #[test]
    fn matches_respects_configured_globs() {
        let value = serde_json::json!({
            "shadow_diagnostics": { "command": ["true"], "globs": ["*.py"] }
        });
        let config = ShadowDiagnosticsConfig::parse(&value).expect("parsed");
        assert!(config.matches("a.py", |pattern, path| pattern == "*.py"
            && path.ends_with(".py")));
        assert!(!config.matches("a.rs", |pattern, path| pattern == "*.py"
            && path.ends_with(".py")));
    }

    #[test]
    fn passing_diagnostics_never_touch_the_real_tree() {
        // verify_candidate only ever writes inside the isolated worktree;
        // the seeded repo's working tree must be untouched either way.
        let root = seeded_repo("pass");
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics": { "command": ["true"] }
        }))
        .expect("parsed");
        let outcome = verify_candidate(&root, "new.txt", b"hello\n", &config);
        assert!(
            matches!(outcome, ShadowVerifyOutcome::Passed { .. }),
            "{outcome:?}"
        );
        assert!(
            !root.join("new.txt").exists(),
            "verify_candidate must never write the real tree"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn multi_candidate_diagnostics_see_all_creations_and_deletions_together() {
        use workspace::multi_edit_recovery::{
            FileImage, RecoveryEntry, RecoveryRecord, RecoveryState,
        };
        let root = seeded_repo("multi-together");
        let image = |bytes: &[u8]| {
            Some(FileImage {
                bytes: bytes.to_vec(),
                mode: Some(0o600),
            })
        };
        let candidate = RecoveryRecord::new(
            RecoveryState::Intent,
            vec![
                RecoveryEntry {
                    path: "seed.txt".into(),
                    before: image(b"seed\n"),
                    after: None,
                },
                RecoveryEntry {
                    path: "a.txt".into(),
                    before: None,
                    after: image(b"ALPHA"),
                },
                RecoveryEntry {
                    path: "b.txt".into(),
                    before: None,
                    after: image(b"BETA"),
                },
            ],
        )
        .unwrap();
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics": { "command": ["sh", "-c", "test ! -e seed.txt && test \"$(cat a.txt)\" = ALPHA && test \"$(cat b.txt)\" = BETA"], "globs": ["*.txt"] }
        })).unwrap();
        let outcome = verify_multi_candidate(&root, &candidate, &config, || false);
        assert!(
            matches!(outcome, ShadowVerifyOutcome::Passed { .. }),
            "{outcome:?}"
        );
        assert_eq!(std::fs::read(root.join("seed.txt")).unwrap(), b"seed\n");
        assert!(!root.join("a.txt").exists());
        assert!(!root.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn multi_candidate_never_follows_a_shadow_worktree_symlink() {
        use workspace::multi_edit_recovery::{
            FileImage, RecoveryEntry, RecoveryRecord, RecoveryState,
        };
        let root = seeded_repo("multi-symlink");
        let external = root.with_extension("outside");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("file.txt"), b"outside").unwrap();
        std::os::unix::fs::symlink(&external, root.join("link")).unwrap();
        git(&root, &["add", "link"]);
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-m",
                "link",
            ],
        );
        let candidate = RecoveryRecord::new(
            RecoveryState::Intent,
            vec![RecoveryEntry {
                path: "link/file.txt".into(),
                before: None,
                after: Some(FileImage {
                    bytes: b"changed".to_vec(),
                    mode: Some(0o600),
                }),
            }],
        )
        .unwrap();
        let config = ShadowDiagnosticsConfig::parse(
            &serde_json::json!({"shadow_diagnostics":{"command":["true"]}}),
        )
        .unwrap();
        let outcome = verify_multi_candidate(&root, &candidate, &config, || false);
        assert!(
            matches!(outcome, ShadowVerifyOutcome::Skipped { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read(external.join("file.txt")).unwrap(),
            b"outside"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(external);
    }

    #[cfg(unix)]
    #[test]
    fn multi_candidate_rejects_diagnostics_that_change_an_edited_file() {
        use workspace::multi_edit_recovery::{
            FileImage, RecoveryEntry, RecoveryRecord, RecoveryState,
        };
        let root = seeded_repo("multi-mutated");
        let candidate = RecoveryRecord::new(
            RecoveryState::Intent,
            vec![RecoveryEntry {
                path: "a.txt".into(),
                before: None,
                after: Some(FileImage {
                    bytes: b"candidate".to_vec(),
                    mode: Some(0o600),
                }),
            }],
        )
        .unwrap();
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics":{"command":["sh","-c","printf changed > a.txt"]}
        }))
        .unwrap();
        let outcome = verify_multi_candidate(&root, &candidate, &config, || false);
        assert!(
            matches!(outcome, ShadowVerifyOutcome::Failed { diagnostics_tail } if diagnostics_tail.contains("modified a candidate"))
        );
        assert!(!root.join("a.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_multi_candidate_never_starts_diagnostics() {
        use workspace::multi_edit_recovery::{
            FileImage, RecoveryEntry, RecoveryRecord, RecoveryState,
        };
        let root = seeded_repo("multi-cancel");
        let candidate = RecoveryRecord::new(
            RecoveryState::Intent,
            vec![RecoveryEntry {
                path: "a.txt".into(),
                before: None,
                after: Some(FileImage {
                    bytes: b"candidate".to_vec(),
                    mode: Some(0o600),
                }),
            }],
        )
        .unwrap();
        let config = ShadowDiagnosticsConfig::parse(
            &serde_json::json!({"shadow_diagnostics":{"command":["false"]}}),
        )
        .unwrap();
        assert!(
            matches!(verify_multi_candidate(&root, &candidate, &config, || true), ShadowVerifyOutcome::Skipped { reason } if reason.contains("cancelled"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(all(
        unix,
        not(any(
            target_os = "cygwin",
            target_os = "horizon",
            target_os = "openbsd",
            target_os = "redox",
            target_os = "wasi"
        ))
    ))]
    #[test]
    fn diagnostics_stop_background_children_on_success_timeout_and_cancel() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let root = seeded_repo("diagnostic-process-group");
        let command = |name: &str, wait: bool| {
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!(
                    "(sleep 0.5; printf leaked > {name}) & {}",
                    if wait { "wait" } else { "exit 0" }
                ),
            ]
        };
        let (ok, _) = run_diagnostics_once(
            &command("success-leak", false),
            &root,
            Duration::from_secs(2),
        );
        assert!(ok);
        let (ok, output) = run_diagnostics_once(
            &command("timeout-leak", true),
            &root,
            Duration::from_millis(40),
        );
        assert!(!ok && output.contains("timed out"), "{output}");
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancelled);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            trigger.store(true, Ordering::SeqCst);
        });
        let (ok, output, uncertain) = run_diagnostics_with_cancel(
            &command("cancel-leak", true),
            &root,
            Duration::from_secs(2),
            || cancelled.load(Ordering::SeqCst),
        );
        assert!(!uncertain);
        thread.join().unwrap();
        assert!(!ok && output.contains("cancelled"), "{output}");
        std::thread::sleep(Duration::from_millis(700));
        for file in ["success-leak", "timeout-leak", "cancel-leak"] {
            assert!(
                !root.join(file).exists(),
                "diagnostics child survived: {file}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failing_diagnostics_are_reported_and_the_real_tree_stays_untouched() {
        let root = seeded_repo("fail");
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics": { "command": ["false"] }
        }))
        .expect("parsed");
        let outcome = verify_candidate(&root, "broken.txt", b"bad\n", &config);
        assert!(
            matches!(outcome, ShadowVerifyOutcome::Failed { .. }),
            "{outcome:?}"
        );
        assert!(!root.join("broken.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn diagnostics_see_the_candidate_content_via_path_substitution() {
        let root = seeded_repo("subst");
        // grep exits 0 only if the candidate content contains MARKER.
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics": { "command": ["grep", "-q", "MARKER", "{path}"] }
        }))
        .expect("parsed");
        let ok = verify_candidate(&root, "check.txt", b"has MARKER inside\n", &config);
        assert!(matches!(ok, ShadowVerifyOutcome::Passed { .. }), "{ok:?}");
        let missing = verify_candidate(&root, "check.txt", b"nothing here\n", &config);
        assert!(
            matches!(missing, ShadowVerifyOutcome::Failed { .. }),
            "{missing:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_candidate_never_leaks_the_worktree_it_created() {
        // The candidate write always dirties the worktree, and
        // GitWorktreeStore::remove_view deliberately refuses to force-delete
        // a dirty one — verify_candidate must discard its own changes before
        // removal, or every call leaks a worktree directory + internal ref.
        let root = seeded_repo("noleak");
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics": { "command": ["true"] }
        }))
        .expect("parsed");
        for _ in 0..3 {
            verify_candidate(&root, "candidate.txt", b"content\n", &config);
        }
        let cancel = CancellationToken::new();
        let store = GitWorktreeStore::open(&root, &cancel).expect("open store");
        let remaining = store.list_views(&cancel).expect("list views");
        assert!(
            remaining.is_empty(),
            "expected no leaked worktrees, found {}: {remaining:?}",
            remaining.len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_git_root_skips_rather_than_panics_or_fabricates_a_pass() {
        let dir = std::env::temp_dir().join(format!("rapidlm-shadow-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics": { "command": ["true"] }
        }))
        .expect("parsed");
        let outcome = verify_candidate(&dir, "a.txt", b"x", &config);
        assert!(
            matches!(outcome, ShadowVerifyOutcome::Skipped { .. }),
            "{outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnostics_tail_survives_a_trailing_invalid_utf8_byte() {
        // A `read_to_string`-based collection fails validity for the *whole*
        // captured file the instant any byte anywhere is invalid UTF-8,
        // discarding an otherwise perfectly good tail rather than just the
        // offending byte. `read_capped_bytes` + this module's `truncate`
        // (already `from_utf8_lossy`-based) must instead preserve the valid
        // prefix.
        let root = seeded_repo("badutf8");
        let config = ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics": { "command": ["printf", "ok-output\\377"] }
        }))
        .expect("parsed");
        let outcome = verify_candidate(&root, "new.txt", b"hello\n", &config);
        match outcome {
            ShadowVerifyOutcome::Passed { diagnostics_tail } => {
                assert!(
                    diagnostics_tail.contains("ok-output"),
                    "expected the valid prefix to survive a trailing invalid UTF-8 byte, got {diagnostics_tail:?}"
                );
            }
            other => panic!("expected Passed (printf always exits 0), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
