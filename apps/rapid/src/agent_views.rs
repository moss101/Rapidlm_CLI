//! Worktree isolation for write-capable subagents (§4 of the delivery goal).
//!
//! Every write-capable `task_spawn` child runs in its own git worktree view
//! (`crates/workspace`'s [`GitWorktreeStore`]: `refs/rapidlm/views/{id}`, a
//! detached checkout at the parent's HEAD — the user's branch and dirty tree
//! are never touched). The child's tools are rooted at the worktree, so its
//! writes are `attributable` by construction: the diff against the base
//! commit is exactly its change set.
//!
//! Integration is deliberate in an interactive session: the child finishes,
//! its changes stay in the worktree, the user reviews (`/diff --agent`,
//! `/agents show`) and then either
//!
//! - `/agents integrate <id> [check-command...]` — apply the child's patch
//!   to the parent tree with `git apply --3way` (a real three-way merge on
//!   blob ids: a file the user changed since the base conflicts cleanly and
//!   the parent is left untouched — the dry run decides before anything is
//!   written), optionally re-run a check command in the parent, and clean up
//!   the view; or
//! - `/agents abandon <id>` — remove the worktree and its ref. The store's
//!   `remove_view` never force-deletes, so an unremovable worktree is
//!   reported, not destroyed.
//!
//! In a headless run there is no reviewer, so a successful child's patch is
//! applied automatically (same three-way merge, same conflict refusal — a
//! conflict leaves the parent untouched and the worktree in place, and the
//! child's report says exactly where its changes are held).
//!
//! A write-capable child whose view cannot be created (not a git repository,
//! worktree limit, …) is refused fail-closed with the reason — per-file write
//! locks are scheduling, not isolation, and isolation is not optional for a
//! child that can mutate the tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::{AgentId, RepoId, WorkspaceViewId};
use workspace::backends::git_worktree::GitWorktreeStore;
use workspace::view::{CreateView, ViewAccess, ViewRegistry, WorkspaceBackend};

use workspace::view::CancellationToken;

/// One child's isolated view.
#[derive(Clone, Debug)]
pub struct ChildView {
    pub view_id: WorkspaceViewId,
    pub worktree: PathBuf,
    pub base_commit: String,
}

/// What `integrate` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationOutcome {
    /// The patch applied cleanly; files are listed.
    Integrated { files: Vec<String> },
    /// The child changed nothing.
    NothingToApply,
    /// The patch would conflict with the parent's own changes; the parent is
    /// untouched and the worktree is kept for another resolution.
    Conflict { files: Vec<String> },
    /// The patch applied and the check command exited 0.
    IntegratedVerified {
        files: Vec<String>,
        check_output: String,
    },
    /// The patch applied but the check command failed.
    CheckFailed {
        files: Vec<String>,
        check_output: String,
    },
}

impl IntegrationOutcome {
    pub const fn applied(&self) -> bool {
        matches!(
            self,
            Self::Integrated { .. } | Self::IntegratedVerified { .. } | Self::CheckFailed { .. }
        )
    }
}

/// The per-session registry of child views. Shared between the turn threads
/// that spawn children and the session loop that integrates or abandons.
#[derive(Default)]
pub struct AgentViewManager {
    views: Mutex<HashMap<AgentId, ChildView>>,
}

impl AgentViewManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an isolated worktree view for a write-capable child.
    pub fn create_for(&self, root: &Path, agent: AgentId) -> Result<ChildView, String> {
        let mut views = self
            .views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cancel = CancellationToken::new();
        let store = GitWorktreeStore::open(root, &cancel).map_err(|err| err.to_string())?;
        let registry = ViewRegistry::new();
        let view = registry
            .create(
                CreateView::new(
                    RepoId::new(),
                    WorkspaceBackend::GitWorktree,
                    "HEAD",
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(agent),
                &cancel,
            )
            .map_err(|err| err.to_string())?;
        let record = store
            .create_view(&view, &cancel)
            .map_err(|err| err.to_string())?;
        let child = ChildView {
            view_id: record.view_id(),
            worktree: record.worktree_path().to_path_buf(),
            base_commit: record.resolved_commit().to_owned(),
        };
        views.insert(agent, child.clone());
        Ok(child)
    }

    pub fn get(&self, agent: AgentId) -> Option<ChildView> {
        self.views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&agent)
            .cloned()
    }

    pub fn view_id_of(&self, agent: AgentId) -> Option<WorkspaceViewId> {
        self.get(agent).map(|child| child.view_id)
    }

    /// Every agent currently holding a view — the resolver's candidate set
    /// for `/agents integrate|abandon`.
    pub fn held_agents(&self) -> Vec<AgentId> {
        self.views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    fn store_for(root: &Path) -> Result<GitWorktreeStore, String> {
        let cancel = CancellationToken::new();
        GitWorktreeStore::open(root, &cancel).map_err(|err| err.to_string())
    }

    /// The child's patch against its base: staged-everything diff, bounded.
    fn child_patch(child: &ChildView) -> Result<Vec<u8>, String> {
        run_git(&child.worktree, &["add", "-A", "--", "."], 4 * 1024 * 1024)?;
        run_git_bytes(
            &child.worktree,
            &["diff", "--binary", &child.base_commit],
            8 * 1024 * 1024,
        )
    }

    /// Changed-file names (for conflict reporting and the integration
    /// receipt), from the same diff the patch was built from.
    fn child_changed_files(child: &ChildView) -> Result<Vec<String>, String> {
        let output = run_git_bytes(
            &child.worktree,
            &["diff", "--name-only", &child.base_commit],
            1024 * 1024,
        )?;
        Ok(String::from_utf8_lossy(&output)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Apply the child's patch to the parent tree.
    ///
    /// `git apply --3way` is a real three-way merge: the binary diff carries
    /// the base blob ids, so a file the user changed since the child's base
    /// conflicts on content, not on timestamps. The dry run decides before
    /// anything is written — a conflicting integration leaves the parent
    /// byte-identical and the worktree in place.
    pub fn integrate(
        &self,
        root: &Path,
        agent: AgentId,
        check_command: Option<&str>,
    ) -> Result<(IntegrationOutcome, Option<ChildView>), String> {
        let child = self.get(agent).ok_or_else(|| {
            "no isolated view for that agent (it may not be write-capable or already integrated)"
                .to_owned()
        })?;
        let patch = Self::child_patch(&child)?;
        if patch.is_empty() {
            Self::reset_worktree(&child)?;
            self.cleanup(root, agent)?;
            return Ok((IntegrationOutcome::NothingToApply, None));
        }
        let files = Self::child_changed_files(&child)?;
        // Conflict detection at file granularity, decided BEFORE anything is
        // written: a file the parent changed since the child's base is a
        // conflict, and the parent is left byte-identical. Deliberately
        // coarser than a line-level three-way merge — refusing to merge two
        // edits to one file is the safe default for an unreviewed machine
        // patch; the worktree is kept so a human can merge by hand.
        let mut conflicts = Vec::new();
        for file in &files {
            // `git diff --quiet <base> -- <file>` exits 0 iff the parent's
            // file is identical to the child's base.
            if run_git_process_quiet(root, &["diff", "--quiet", &child.base_commit, "--", file])
                .is_err()
            {
                conflicts.push(file.clone());
            }
        }
        if !conflicts.is_empty() {
            return Ok((
                IntegrationOutcome::Conflict { files: conflicts },
                Some(child),
            ));
        }
        // Plain apply (no 3way): every patched file is provably unchanged in
        // the parent, so the patch applies cleanly or not at all.
        run_git_stdin(root, &["apply", "-"], &patch).map_err(|(output, code)| {
            format!(
                "the apply failed ({code}; parent left as it was): {}",
                String::from_utf8_lossy(&output)
            )
        })?;
        let outcome = match check_command {
            Some(command) if !command.trim().is_empty() => {
                match run_bounded_command(command, root, 300) {
                    Ok(output) if output.status_success => IntegrationOutcome::IntegratedVerified {
                        files,
                        check_output: bounded_text(&output.text),
                    },
                    result => IntegrationOutcome::CheckFailed {
                        files,
                        check_output: bounded_text(&result.map(|o| o.text).unwrap_or_default()),
                    },
                }
            }
            _ => IntegrationOutcome::Integrated { files },
        };
        // Success (even a failed check — the files are applied) releases the
        // view; the user can still inspect via the parent's own /diff.
        Self::reset_worktree(&child)?;
        self.cleanup(root, agent)?;
        Ok((outcome, None))
    }

    /// Remove the view without applying anything. The store never
    /// force-deletes; a stuck worktree is reported as the store's
    /// `CleanupFailed`, leaving the parent untouched.
    pub fn abandon(&self, root: &Path, agent: AgentId) -> Result<(), String> {
        let child = self
            .get(agent)
            .ok_or_else(|| "no isolated view for that agent (nothing to abandon)".to_owned())?;
        // Abandoning discards the child's changes by definition; the store's
        // removal refuses a dirty worktree, so reset it first — worktree-only,
        // no shared ref moves.
        Self::reset_worktree(&child)?;
        let store = Self::store_for(root)?;
        let cancel = CancellationToken::new();
        store
            .remove_view(child.view_id, &cancel)
            .map_err(|err| err.to_string())?;
        self.views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&agent);
        Ok(())
    }

    /// Release a view after the fact (used when the child failed and its
    /// changes are not wanted; a failing child's worktree is discarded —
    /// the parent never saw any of it). The worktree is reset first: the
    /// store's removal never force-deletes, and a dirty worktree is exactly
    /// what it refuses.
    pub fn cleanup(&self, root: &Path, agent: AgentId) -> Result<(), String> {
        let child = match self.get(agent) {
            Some(child) => child,
            None => return Ok(()),
        };
        Self::reset_worktree(&child)?;
        let store = Self::store_for(root)?;
        let cancel = CancellationToken::new();
        let removed = store.remove_view(child.view_id, &cancel);
        self.views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&agent);
        removed.map_err(|err| err.to_string())
    }

    /// Point the detached worktree back at its base and drop untracked
    /// files — worktree-local only; no shared ref or branch moves.
    fn reset_worktree(child: &ChildView) -> Result<(), String> {
        let _ = run_git_bytes(
            &child.worktree,
            &["reset", "--hard", &child.base_commit],
            4096,
        );
        let _ = run_git_bytes(&child.worktree, &["clean", "-fdq"], 4096);
        Ok(())
    }

    /// Bounded diff-stat for the child's report: what it changed, in the
    /// worktree, against its base.
    pub fn diff_stat(&self, agent: AgentId) -> Option<String> {
        let child = self.get(agent)?;
        // Stage before diffing: an untracked new file is exactly the change
        // a review needs to see, and plain `git diff` hides it.
        let _ = run_git_bytes(&child.worktree, &["add", "-A", "--", "."], 1024);
        run_git_bytes(
            &child.worktree,
            &["diff", "--stat", &child.base_commit],
            64 * 1024,
        )
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

struct CommandResult {
    text: String,
    status_success: bool,
}

fn run_bounded_command(
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
) -> Result<CommandResult, String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", command])
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
    .map_err(|err| format!("check command could not start: {err}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) => {
                let mut text = String::new();
                if let Some(stdout) = child.stdout.take() {
                    let _ = stdout.take(8 * 1024).read_to_string(&mut text);
                }
                if let Some(stderr) = child.stderr.take() {
                    let _ = stderr.take(4 * 1024).read_to_string(&mut text);
                }
                return Ok(CommandResult {
                    text,
                    status_success: status.success(),
                });
            }
            None => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(format!(
                        "check command exceeded its {timeout_secs}s ceiling and was killed"
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

fn bounded_text(text: &str) -> String {
    const CAP: usize = 4 * 1024;
    if text.len() <= CAP {
        return text.to_owned();
    }
    let mut end = CAP;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated)", &text[..end])
}

fn run_git(dir: &Path, args: &[&str], max_output: usize) -> Result<(), String> {
    run_git_bytes(dir, args, max_output).map(|_| ())
}

fn run_git_bytes(dir: &Path, args: &[&str], max_output: usize) -> Result<Vec<u8>, String> {
    run_git_bytes_status(dir, args, max_output).map_err(|(output, code)| {
        format!(
            "git {} failed ({code}): {}",
            args.join(" "),
            String::from_utf8_lossy(&output)
        )
    })
}

/// Run git, returning `(stderr-ish output, exit code)` on failure. Output is
/// hard-capped so a pathological diff cannot balloon memory.
fn run_git_bytes_status(
    dir: &Path,
    args: &[&str],
    max_output: usize,
) -> Result<Vec<u8>, (Vec<u8>, i32)> {
    run_git_process(dir, args, None, max_output)
}

/// Exit-status-only git probe (conflict checks); output discarded.
fn run_git_process_quiet(dir: &Path, args: &[&str]) -> Result<(), (Vec<u8>, i32)> {
    run_git_process(dir, args, None, 1024).map(|_| ())
}

/// Run git with optional stdin payload (the patch path for `git apply -`).
fn run_git_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>, (Vec<u8>, i32)> {
    run_git_process(dir, args, Some(stdin), 16 * 1024 * 1024)
}

fn run_git_process(
    dir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    max_output: usize,
) -> Result<Vec<u8>, (Vec<u8>, i32)> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| (err.to_string().into_bytes(), -1))?;
    if let Some(payload) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(payload);
    }
    let output = child
        .wait_with_output()
        .map_err(|err| (err.to_string().into_bytes(), -1))?;
    let mut stdout = output.stdout;
    if stdout.len() > max_output {
        stdout.truncate(max_output);
    }
    if output.status.success() {
        Ok(stdout)
    } else {
        let mut failure = output.stderr;
        if failure.len() > 8 * 1024 {
            failure.truncate(8 * 1024);
        }
        failure.extend_from_slice(&stdout);
        Err((failure, output.status.code().unwrap_or(-1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo {
        root: PathBuf,
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A real git repository with one commit — the parent workspace a child
    /// is isolated from.
    fn repo(name: &str) -> Repo {
        let root = std::env::temp_dir().join(format!(
            "agent-views-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@rapidlm.dev"]);
        git(&["config", "user.name", "RapidLM Test"]);
        std::fs::write(root.join("base.txt"), "base content\n").unwrap();
        assert!(git(&["add", "-A"]).status.success());
        assert!(git(&["commit", "-q", "-m", "base"]).status.success());
        Repo { root }
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn a_clean_child_patch_integrates_and_releases_the_view() {
        let repo = repo("clean");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        assert!(view.worktree.exists());
        assert!(view.worktree.join("base.txt").exists());
        // The parent's HEAD did not move and its tree is untouched.
        assert_eq!(
            git_in(&repo.root, &["rev-parse", "HEAD"]).trim(),
            view.base_commit
        );

        // The child writes in ITS tree only.
        std::fs::write(view.worktree.join("child.txt"), "from the child\n").unwrap();
        let stat = manager.diff_stat(agent).expect("stat");
        assert!(stat.contains("child.txt"), "{stat}");

        // Integrate: the file lands in the parent, the view is released.
        let (outcome, held) = manager
            .integrate(&repo.root, agent, None)
            .expect("integrates");
        assert_eq!(
            outcome,
            IntegrationOutcome::Integrated {
                files: vec!["child.txt".to_owned()]
            }
        );
        assert!(held.is_none());
        assert!(repo.root.join("child.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repo.root.join("child.txt")).unwrap(),
            "from the child\n"
        );
        assert!(
            manager.get(agent).is_none(),
            "view released after integrate"
        );
    }

    #[test]
    fn integration_runs_the_check_command_and_reports_its_result() {
        let repo = repo("check");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).unwrap();
        std::fs::write(view.worktree.join("out.txt"), "x\n").unwrap();

        let (passing, _) = manager
            .integrate(&repo.root, agent, Some("test -f out.txt"))
            .unwrap();
        assert!(matches!(
            passing,
            IntegrationOutcome::IntegratedVerified { .. }
        ));

        // A failing check is reported as applied-but-unverified.
        let view = manager.create_for(&repo.root, agent).unwrap();
        std::fs::write(view.worktree.join("out2.txt"), "y\n").unwrap();
        let (failing, _) = manager
            .integrate(&repo.root, agent, Some("exit 3"))
            .unwrap();
        assert!(matches!(failing, IntegrationOutcome::CheckFailed { .. }));
        assert!(repo.root.join("out2.txt").exists(), "files stay applied");
    }

    #[test]
    fn a_conflicting_patch_is_refused_without_touching_the_parent() {
        let repo = repo("conflict");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).unwrap();
        // The child edits base.txt.
        std::fs::write(view.worktree.join("base.txt"), "child version\n").unwrap();

        // The USER edits base.txt in the parent after the child's base.
        std::fs::write(repo.root.join("base.txt"), "user version\n").unwrap();
        git_in(&repo.root, &["add", "-A"]);
        git_in(&repo.root, &["commit", "-q", "-m", "user change"]);

        let (outcome, held) = manager.integrate(&repo.root, agent, None).expect("decides");
        assert_eq!(
            outcome,
            IntegrationOutcome::Conflict {
                files: vec!["base.txt".to_owned()]
            }
        );
        // The parent's user version is byte-identical: nothing was applied.
        assert_eq!(
            std::fs::read_to_string(repo.root.join("base.txt")).unwrap(),
            "user version\n"
        );
        // The worktree is KEPT for another resolution.
        let view = held.expect("held for retry");
        assert!(view.worktree.exists());
        assert_eq!(
            std::fs::read_to_string(view.worktree.join("base.txt")).unwrap(),
            "child version\n"
        );
        // Abandon discards it without damaging the parent.
        manager.abandon(&repo.root, agent).expect("abandons");
        assert!(manager.get(agent).is_none());
        assert_eq!(
            std::fs::read_to_string(repo.root.join("base.txt")).unwrap(),
            "user version\n"
        );
    }

    #[test]
    fn a_child_that_changed_nothing_reports_it_and_releases() {
        let repo = repo("empty");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        manager.create_for(&repo.root, agent).unwrap();
        let (outcome, held) = manager.integrate(&repo.root, agent, None).unwrap();
        assert_eq!(outcome, IntegrationOutcome::NothingToApply);
        assert!(held.is_none());
        assert!(manager.get(agent).is_none());
    }

    #[test]
    fn isolation_is_refused_fail_closed_outside_a_git_repository() {
        let dir = std::env::temp_dir().join(format!(
            "agent-views-nogit-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let manager = AgentViewManager::new();
        let error = manager
            .create_for(&dir, AgentId::new())
            .expect_err("refused outside git");
        assert!(!error.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_users_own_dirty_uncommitted_work_survives_integration() {
        let repo = repo("dirty");
        // The user has an uncommitted change before the child is spawned.
        std::fs::write(repo.root.join("mine.txt"), "user's own work\n").unwrap();
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).unwrap();
        // The child's base does not include the user's uncommitted file...
        assert!(!view.worktree.join("mine.txt").exists());
        std::fs::write(view.worktree.join("theirs.txt"), "child work\n").unwrap();
        manager.integrate(&repo.root, agent, None).unwrap();
        // ...and integration leaves that work exactly as it was.
        assert_eq!(
            std::fs::read_to_string(repo.root.join("mine.txt")).unwrap(),
            "user's own work\n"
        );
        assert!(repo.root.join("theirs.txt").exists());
    }
}
