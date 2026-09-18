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

/// One entry of the child's `git diff --raw` against its base.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ChildChange {
    /// `A`dded, `M`odified, `D`eleted, `R`enamed, `C`opied, `T`ype-changed.
    status: char,
    /// The destination file mode; `120000` is a symlink, `100755` executable.
    dst_mode: String,
    path: String,
    /// The source path of a rename or copy.
    source: Option<String>,
}

/// A publication staged and committed in the transaction manager, waiting to
/// be written into the checkout. Held together because materializing needs
/// the same manager that issued the receipt.
struct StagedPublication {
    manager: workspace::transaction::TransactionManager,
    receipt: workspace::transaction::CommitReceipt,
}

/// Whether a path is executable. Always false off Unix, where the bit is not
/// part of a file's identity.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
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

    /// Stage the child's changes as a workspace transaction and commit it,
    /// leaving the publication parent-visible but not yet written.
    ///
    /// Every changed path becomes whole-file ops: a delete carrying the
    /// parent's preimage hash, a create carrying the child's bytes, or both
    /// for a modification. Whole-file rather than ranged edits because the
    /// child's diff may be binary and because a mode change must survive;
    /// the preimage is what makes the transaction refuse a parent that moved
    /// under it, a stronger guarantee than the per-file check above.
    fn stage_publication(
        root: &Path,
        child: &ChildView,
        changes: &[ChildChange],
        agent: AgentId,
        parent_revision: &str,
    ) -> Result<StagedPublication, String> {
        use workspace::patch::model::{PatchOp, SemanticPatch};

        let cancel = CancellationToken::new();
        let repo = RepoId::new();
        let registry = ViewRegistry::new();
        // The child is the git worktree the agent wrote in, and carries no
        // write owner so it can be quiesced — a patch is previewed only
        // against a view that has stopped changing. The parent is the
        // project checkout, owned for the duration of the publication.
        let child_view = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::GitWorktree,
                    parent_revision,
                    ViewAccess::ReadWrite,
                ),
                &cancel,
            )
            .map_err(|err| format!("staging child view: {err}"))?;
        let parent_view = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::Direct,
                    parent_revision,
                    ViewAccess::ReadWrite,
                )
                .with_write_owner(agent),
                &cancel,
            )
            .map_err(|err| format!("staging parent view: {err}"))?;
        let child_view = registry
            .quiesce(child_view.id(), &cancel)
            .map_err(|err| format!("quiesce: {err}"))?;

        // Every path git named must produce an op. A path that is readable
        // nowhere is never benign — it means the listing and the trees
        // disagree — so it is an error rather than a silent no-op, which is
        // how a change could previously vanish while the run reported
        // success and then deleted the worktree holding it.
        let mut ops = Vec::new();
        let delete = |path: &str| -> Result<PatchOp, String> {
            let repo_path = protocol::RepoPath::parse(path)
                .map_err(|_| format!("'{path}' is not a repository-relative path"))?;
            let before = std::fs::read(root.join(path))
                .map_err(|err| format!("reading the parent's '{path}': {err}"))?;
            Ok(PatchOp::delete_file(
                repo_path,
                protocol::ArtifactId::from_bytes(&before),
            ))
        };
        let create = |path: &str| -> Result<PatchOp, String> {
            let repo_path = protocol::RepoPath::parse(path)
                .map_err(|_| format!("'{path}' is not a repository-relative path"))?;
            let source = child.worktree.join(path);
            let after = std::fs::read(&source)
                .map_err(|err| format!("reading the child's '{path}': {err}"))?;
            PatchOp::create_file(repo_path, after, is_executable(&source))
                .map_err(|err| format!("staging '{path}': {err:?}"))
        };

        for change in changes {
            // A symlink's bytes are its target, so publishing it as a
            // regular file would silently turn a link into a copy. The
            // overlay has no symlink slot, so this is refused rather than
            // quietly changed.
            if change.dst_mode == "120000" {
                return Err(format!(
                    "'{}' is a symlink; publishing symlinks is not supported yet, \
                     so the view is kept for a manual merge",
                    change.path
                ));
            }
            match change.status {
                // A rename is the one case the old listing could not even
                // see: the source must be deleted, not left behind.
                'R' => {
                    let source = change
                        .source
                        .as_deref()
                        .ok_or_else(|| "a rename without a source".to_owned())?;
                    ops.push(delete(source)?);
                    ops.push(create(&change.path)?);
                }
                'C' => ops.push(create(&change.path)?),
                'D' => ops.push(delete(&change.path)?),
                'A' => ops.push(create(&change.path)?),
                // Modified or type-changed: whole-file replacement, so a
                // binary body and a mode change both survive.
                _ => {
                    ops.push(delete(&change.path)?);
                    ops.push(create(&change.path)?);
                }
            }
        }

        let patch = SemanticPatch::new(ops, agent, parent_revision, &cancel)
            .map_err(|err| format!("staging patch: {err:?}"))?;
        let empty = SemanticPatch::new(Vec::new(), agent, parent_revision, &cancel)
            .map_err(|err| format!("staging patch: {err:?}"))?;
        let preview =
            workspace::merge::preview_merge(&child_view, &parent_view, &patch, &empty, &cancel)
                .map_err(|err| format!("merge preview: {err:?}"))?;
        if !preview.is_conflict_free() {
            return Err("the staged publication conflicts with the parent".to_owned());
        }
        let backend = workspace::backends::direct::DirectBackend::open_with(
            root,
            parent_view.clone(),
            workspace::backends::direct::DirectOptions::interactive(),
            &cancel,
        )
        .map_err(|err| format!("workspace backend: {err}"))?;
        let manager = workspace::transaction::TransactionManager::new();
        let tx = manager
            .begin_transaction(&preview, &parent_view, &backend, agent, &cancel)
            .map_err(|err| format!("begin publication: {err}"))?;
        // The transaction's own required checks gate visibility: the parent
        // revision must still match and every preimage must still hold. The
        // caller's check command is deliberately NOT a hook — a hook runs
        // before the checkout is written, so a command that reads the real
        // tree would test the unmodified files. It runs after materializing,
        // as it always has.
        let receipt = manager
            .commit_transaction(
                tx,
                &parent_view,
                &[&workspace::transaction::AcceptHook],
                &cancel,
            )
            .map_err(|err| format!("publication refused: {err}"))?;
        Ok(StagedPublication { manager, receipt })
    }

    /// The parent's current revision — the baseline a publication is frozen
    /// against, and part of its effect identity.
    fn parent_revision(root: &Path) -> Result<String, String> {
        let out = run_git_bytes(root, &["rev-parse", "HEAD"], 1024)?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
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

    /// What the child changed, as structured entries.
    ///
    /// `--raw -z -M` rather than `--name-only`, for three reasons that each
    /// cost correctness under the older listing:
    ///
    /// - a **rename** prints only its destination under `--name-only`, so
    ///   reconstructing a publication from that list created the new file and
    ///   left the old one behind;
    /// - a path with any non-ASCII byte is **quoted** by default
    ///   (`"caf\303\251.rs"`), which is not the path and reads nowhere; `-z`
    ///   emits raw bytes with NUL separators and no quoting;
    /// - the raw format carries the **file modes**, so a symlink (`120000`)
    ///   is recognisable instead of being silently published as a regular
    ///   file containing its target's bytes.
    fn child_changes(child: &ChildView) -> Result<Vec<ChildChange>, String> {
        let output = run_git_bytes(
            &child.worktree,
            &["diff", "--raw", "-z", "-M", &child.base_commit],
            4 * 1024 * 1024,
        )?;
        let mut fields = output.split(|b| *b == 0).filter(|f| !f.is_empty());
        let mut changes = Vec::new();
        while let Some(meta) = fields.next() {
            if !meta.starts_with(b":") {
                // Every entry begins with the `:<modes> <shas> <status>`
                // field; anything else means the format changed under us.
                return Err("unrecognized git raw diff output".to_owned());
            }
            let meta = String::from_utf8_lossy(meta).into_owned();
            let mut parts = meta.split_whitespace();
            let _src_mode = parts.next().unwrap_or_default().trim_start_matches(':');
            let dst_mode = parts.next().unwrap_or_default().to_owned();
            let _src_sha = parts.next();
            let _dst_sha = parts.next();
            let status = parts.next().unwrap_or_default().to_owned();
            let first = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .ok_or_else(|| "git raw diff entry without a path".to_owned())?;
            // Rename and copy carry a second path: source first, then
            // destination.
            let renamed = status.starts_with('R') || status.starts_with('C');
            let (source, path) = if renamed {
                let second = fields
                    .next()
                    .map(|f| String::from_utf8_lossy(f).into_owned())
                    .ok_or_else(|| "git rename entry without a destination".to_owned())?;
                (Some(first), second)
            } else {
                (None, first)
            };
            changes.push(ChildChange {
                status: status.chars().next().unwrap_or('M'),
                dst_mode,
                path,
                source,
            });
        }
        Ok(changes)
    }

    /// Every path a change set touches, destination and rename source alike —
    /// what conflict reporting and the integration receipt name.
    fn changed_paths(changes: &[ChildChange]) -> Vec<String> {
        let mut paths = Vec::new();
        for change in changes {
            if let Some(source) = &change.source {
                paths.push(source.clone());
            }
            paths.push(change.path.clone());
        }
        paths.sort();
        paths.dedup();
        paths
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
        let changes = Self::child_changes(&child)?;
        let files = Self::changed_paths(&changes);
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
        // Publication is an at-most-once effect with a receipt: the journal
        // records prepared → executing → committed around the one write that
        // reaches the user's tree, so a repeat is answered from the journal
        // and a crash mid-apply is reconciled rather than replayed. A
        // publication that cannot be journaled does not happen.
        let parent_revision = Self::parent_revision(root)?;
        let journal = crate::publication::PublicationJournal::for_project(
            &crate::interactive::project_ledger_path(
                &root.join(crate::interactive::PROJECT_MARKER),
            ),
        )
        .map_err(|err| err.to_string())?;
        let request = crate::publication::PublicationRequest {
            principal: &agent.to_string(),
            parent_revision: &parent_revision,
            base_revision: &child.base_commit,
            patch: &patch,
            change_count: files.len(),
        };
        // Settle what a previous crashed run left in flight for *this exact
        // effect* before attempting it again, or a publication interrupted
        // mid-apply would block this one forever with `InFlight`. Scoped to
        // this request's fingerprint: the publication session is
        // project-wide, and this caller holds evidence about its own patch
        // only. Whether that patch landed is decided by the tree itself — a
        // patch that still reverse-applies is present.
        let settle_root = root.to_path_buf();
        let settle_patch = patch.clone();
        let fingerprint = request.fingerprint().map_err(|err| err.to_string())?;
        let report = crate::publication::recover(&journal, fingerprint, |_record| {
            match run_git_stdin(
                &settle_root,
                &["apply", "--check", "--reverse", "-"],
                &settle_patch,
            ) {
                // It reverse-applies, so the change is present: it landed.
                Ok(_) => crate::publication::Settlement::Applied,
                Err(_) => crate::publication::Settlement::NotApplied,
            }
        })
        .map_err(|err| err.to_string())?;
        if !report.undecided.is_empty() {
            return Err(format!(
                "a previous publication of this patch is in an undecided state ({} operation(s)); \
                 it must be settled before integrating again",
                report.undecided.len()
            ));
        }
        // Publication goes through `workspace::TransactionManager` (ADR 0021
        // §4): the child's changes are staged against the parent as a
        // semantic patch, the transaction's required checks re-verify the
        // parent revision and every preimage, and only a committed
        // transaction is written into the checkout. The `CommitReceipt` —
        // parent revision plus patch hash — is the publication receipt, and
        // the journal records the one materializing write as the
        // at-most-once effect.
        let staged = Self::stage_publication(root, &child, &changes, agent, &parent_revision)?;
        let published = crate::publication::publish(&journal, &request, || {
            staged
                .manager
                .materialize(&staged.receipt, root, &CancellationToken::new())
                .map(|_| ())
                .map_err(|err| {
                    let detail = format!("the publication could not be written: {err}");
                    // A partly-written publication is not "nothing happened":
                    // the journal must record it as uncertain so recovery
                    // settles it instead of a retry republishing over it.
                    match err {
                        workspace::transaction::TransactionError::MaterializePartial { .. } => {
                            crate::publication::EffectFailure::Partial(detail)
                        }
                        _ => crate::publication::EffectFailure::Untouched(detail),
                    }
                })
        })
        .map_err(|err| err.to_string())?;
        let _receipt = match published {
            crate::publication::Published::Applied(receipt) => receipt,
            // The same patch onto the same parent revision was already
            // applied; the files are in the tree and re-applying would
            // double-apply. Release the view as a successful integration.
            crate::publication::Published::AlreadyApplied(receipt) => receipt,
        };
        // The publication changed the user's tree, and `git apply` does not
        // go through `workspace_write`'s hook — so without this, integrating
        // a child's patch left every recorded check looking fresh against
        // code it never saw. Each changed file is offered as a subject; the
        // hook stales that subject's own records and then sweeps the rest.
        // Best effort: a failed invalidation must not undo a publication
        // that already happened, but it is reported.
        let invalidator = crate::interactive::evidence_invalidator_for(root);
        for file in &files {
            if let Err(err) = invalidator(file) {
                eprintln!("warning: evidence invalidation after integrate failed: {err}");
                break;
            }
        }
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
        // Byte-exact content assertions below: Git for Windows defaults to
        // `core.autocrlf=true`, which would check `\n` out as `\r\n`.
        git(&["config", "core.autocrlf", "false"]);
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
    fn publication_is_journaled_prepared_executing_committed_and_never_applies_twice() {
        use crate::publication::{PublicationJournal, PublicationRequest, Published, publish};
        use event_ledger::journal::OperationState;

        let repo = repo("publication-journal");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        std::fs::write(view.worktree.join("child.txt"), "from the child\n").unwrap();
        let parent_before = git_in(&repo.root, &["rev-parse", "HEAD"]).trim().to_owned();

        let (outcome, held) = manager
            .integrate(&repo.root, agent, None)
            .expect("integrates");
        assert!(matches!(outcome, IntegrationOutcome::Integrated { .. }));
        assert!(held.is_none());
        assert!(repo.root.join("child.txt").exists());

        // The publication is durable, terminal and committed.
        let ledger_path = crate::interactive::project_ledger_path(
            &repo.root.join(crate::interactive::PROJECT_MARKER),
        );
        let journal = PublicationJournal::for_project(&ledger_path).expect("journal");
        let patch = std::fs::read_to_string(repo.root.join("child.txt")).unwrap();
        assert!(!patch.is_empty());

        // Re-publishing the SAME effect is answered from the journal: the
        // closure that would write must not run a second time.
        let recorded_patch = b"the recorded patch bytes";
        let request = PublicationRequest {
            principal: &agent.to_string(),
            parent_revision: &parent_before,
            base_revision: &view.base_commit,
            patch: recorded_patch,
            change_count: 1,
        };
        let first = publish(&journal, &request, || Ok(())).expect("first");
        let receipt = match first {
            Published::Applied(receipt) => receipt,
            other => panic!("expected a fresh apply, got {other:?}"),
        };
        assert_eq!(receipt.parent_revision, parent_before);
        assert_eq!(receipt.change_count, 1);
        assert_eq!(
            journal
                .journal()
                .load(receipt.operation, &Default::default())
                .expect("load")
                .state(),
            OperationState::Committed
        );

        let again = publish(&journal, &request, || {
            panic!("an already-committed effect must not run again")
        })
        .expect("answered from the journal");
        match again {
            Published::AlreadyApplied(second) => {
                assert_eq!(second.operation, receipt.operation, "same operation");
                assert_eq!(second.patch_hash, receipt.patch_hash);
            }
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }

        // A different parent revision is a different effect and may run.
        let moved = PublicationRequest {
            parent_revision: "0000000000000000000000000000000000000000",
            ..request
        };
        assert!(matches!(
            publish(&journal, &moved, || Ok(())).expect("distinct effect"),
            Published::Applied(_)
        ));
    }

    #[test]
    fn recovery_settles_an_interrupted_publication_instead_of_replaying_it() {
        use crate::publication::{
            PublicationError, PublicationJournal, PublicationRequest, Settlement, publish, recover,
        };
        use event_ledger::journal::{OperationState, ReplayPolicy};

        let repo = repo("publication-recovery");
        let ledger_path = crate::interactive::project_ledger_path(
            &repo.root.join(crate::interactive::PROJECT_MARKER),
        );
        let journal = PublicationJournal::for_project(&ledger_path).expect("journal");
        let request = PublicationRequest {
            principal: "agent",
            parent_revision: "rev-1",
            base_revision: "base-1",
            patch: b"patch",
            change_count: 1,
        };

        // A publication that dies mid-apply: the closure panics the process
        // in reality; here it leaves the record `Executing` by failing to
        // reach either terminal call. We simulate by driving the journal
        // directly to the same state the crash leaves behind.
        let spec = event_ledger::journal::EffectSpec::new(
            crate::publication::PUBLISH_ACTION,
            "agent",
            "rev-1",
            format!(
                "base=base-1 patch={}",
                protocol::ArtifactId::from_bytes(b"patch")
            ),
        )
        .expect("spec");
        let record = journal
            .journal()
            .prepare(
                journal.session(),
                &spec,
                event_ledger::journal::IdempotencyClass::AtMostOnce,
                &Default::default(),
            )
            .expect("prepare");
        journal
            .journal()
            .mark_executing(record.id(), &Default::default())
            .expect("executing");

        // An at-most-once effect in flight may never be replayed.
        assert_eq!(
            journal
                .journal()
                .load(record.id(), &Default::default())
                .expect("load")
                .replay_policy(),
            ReplayPolicy::Reconcile
        );
        // And the next publication of the same effect refuses rather than
        // risking a double-apply.
        assert!(matches!(
            publish(&journal, &request, || Ok(())),
            Err(PublicationError::InFlight { .. })
        ));

        // Recovery settles it from the world's own evidence — and only the
        // effect the caller can evidence.
        let report = recover(&journal, record.fingerprint(), |_record| {
            Settlement::Applied
        })
        .expect("recover");
        assert_eq!(report.reconciled, vec![record.id()]);
        assert!(report.undecided.is_empty());
        assert_eq!(
            journal
                .journal()
                .load(record.id(), &Default::default())
                .expect("load")
                .state(),
            OperationState::Reconciled,
            "terminal, so nothing is left in flight"
        );

        // An undecidable one is left for a human, not guessed.
        let second = journal
            .journal()
            .prepare(
                journal.session(),
                &event_ledger::journal::EffectSpec::new(
                    crate::publication::PUBLISH_ACTION,
                    "agent",
                    "rev-2",
                    "base=base-2 patch=x",
                )
                .expect("spec"),
                event_ledger::journal::IdempotencyClass::AtMostOnce,
                &Default::default(),
            )
            .expect("prepare");
        journal
            .journal()
            .mark_executing(second.id(), &Default::default())
            .expect("executing");
        let report = recover(&journal, second.fingerprint(), |_record| {
            Settlement::Unknown
        })
        .expect("recover");
        assert_eq!(report.undecided, vec![second.id()]);
        assert_eq!(
            journal
                .journal()
                .load(second.id(), &Default::default())
                .expect("load")
                .state(),
            OperationState::Uncertain,
            "an undecidable effect stays uncertain rather than being guessed"
        );
    }

    #[test]
    fn publication_goes_through_a_workspace_transaction_and_its_receipt() {
        // ADR 0021 §4: a candidate reaches the user's tree only through
        // `TransactionManager` — staged, required-checks-verified, committed,
        // then materialized. The receipt is the parent revision the patch was
        // published onto plus the patch hash.
        let repo = repo("publication-transaction");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        std::fs::write(view.worktree.join("child.txt"), "from the child\n").unwrap();
        // A binary file and an executable, to prove whole-file staging keeps
        // both — a ranged text edit could carry neither.
        std::fs::write(view.worktree.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        let parent_before = git_in(&repo.root, &["rev-parse", "HEAD"]).trim().to_owned();

        let staged = AgentViewManager::stage_publication(
            &repo.root,
            &view,
            &[
                ChildChange {
                    status: 'A',
                    dst_mode: "100644".to_owned(),
                    path: "child.txt".to_owned(),
                    source: None,
                },
                ChildChange {
                    status: 'A',
                    dst_mode: "100644".to_owned(),
                    path: "blob.bin".to_owned(),
                    source: None,
                },
            ],
            agent,
            &parent_before,
        )
        .expect("stages");
        // Committing alone leaves the checkout untouched.
        assert!(!repo.root.join("child.txt").exists());
        assert_eq!(staged.receipt.parent_revision(), parent_before);
        assert_eq!(staged.receipt.change_count(), 2);

        // Materializing is what writes the files.
        let written = staged
            .manager
            .materialize(&staged.receipt, &repo.root, &CancellationToken::new())
            .expect("materialize");
        assert_eq!(written, 2);
        assert_eq!(
            std::fs::read_to_string(repo.root.join("child.txt")).unwrap(),
            "from the child\n"
        );
        assert_eq!(
            std::fs::read(repo.root.join("blob.bin")).unwrap(),
            vec![0u8, 159, 146, 150],
            "a non-UTF-8 file survives the staged publication"
        );
    }

    #[test]
    fn a_rename_publishes_as_a_rename_and_does_not_leave_the_old_file_behind() {
        // `git diff --name-only` prints only a rename's destination, so a
        // publication reconstructed from that list created the new file and
        // left the old one in the parent — a duplicate module, reported as a
        // clean integration, with the worktree holding the truth deleted.
        let repo = repo("publication-rename");
        std::fs::write(repo.root.join("old.txt"), "content\n").unwrap();
        git_in(&repo.root, &["add", "-A"]);
        git_in(&repo.root, &["commit", "-qm", "add old"]);

        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        git_in(&view.worktree, &["mv", "old.txt", "new.txt"]);

        let (outcome, held) = manager
            .integrate(&repo.root, agent, None)
            .expect("integrates");
        assert!(
            matches!(outcome, IntegrationOutcome::Integrated { .. }),
            "{outcome:?}"
        );
        assert!(held.is_none());
        assert!(
            repo.root.join("new.txt").exists(),
            "the destination must be published"
        );
        assert!(
            !repo.root.join("old.txt").exists(),
            "the rename source must be removed, not left behind"
        );
    }

    #[test]
    fn a_non_ascii_path_is_published_rather_than_silently_dropped() {
        // git quotes non-ASCII paths by default (`"caf\303\251.rs"`), which
        // is not a path and reads nowhere — the change used to vanish while
        // the run reported success and deleted the worktree.
        let repo = repo("publication-unicode");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        std::fs::write(view.worktree.join("café.rs"), "fn main() {}\n").unwrap();

        let (outcome, _held) = manager
            .integrate(&repo.root, agent, None)
            .expect("integrates");
        assert!(
            matches!(outcome, IntegrationOutcome::Integrated { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("café.rs")).expect("published"),
            "fn main() {}\n"
        );
    }

    #[test]
    fn a_modified_and_a_deleted_file_both_publish() {
        // The common case — the child edits an existing file — plus a
        // deletion, neither of which any earlier test exercised.
        let repo = repo("publication-modify");
        std::fs::write(repo.root.join("keep.txt"), "before\n").unwrap();
        std::fs::write(repo.root.join("gone.txt"), "doomed\n").unwrap();
        git_in(&repo.root, &["add", "-A"]);
        git_in(&repo.root, &["commit", "-qm", "seed"]);

        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        std::fs::write(view.worktree.join("keep.txt"), "after\n").unwrap();
        std::fs::remove_file(view.worktree.join("gone.txt")).unwrap();

        let (outcome, _held) = manager
            .integrate(&repo.root, agent, None)
            .expect("integrates");
        assert!(
            matches!(outcome, IntegrationOutcome::Integrated { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("keep.txt")).expect("modified"),
            "after\n",
            "a modification must reach the parent"
        );
        assert!(
            !repo.root.join("gone.txt").exists(),
            "a deletion must reach the parent"
        );
    }

    #[test]
    fn a_symlink_is_refused_rather_than_published_as_a_regular_file() {
        // A symlink's bytes are its target, so publishing it as a regular
        // file silently turns a link into a copy. The overlay has no symlink
        // slot, so the publication is refused and the view kept.
        #[cfg(unix)]
        {
            let repo = repo("publication-symlink");
            let manager = AgentViewManager::new();
            let agent = AgentId::new();
            let view = manager.create_for(&repo.root, agent).expect("view");
            std::fs::write(view.worktree.join("target.txt"), "real\n").unwrap();
            std::os::unix::fs::symlink("target.txt", view.worktree.join("link.txt")).unwrap();

            let err = manager
                .integrate(&repo.root, agent, None)
                .expect_err("refuses a symlink");
            assert!(err.contains("symlink"), "{err}");
            assert!(
                !repo.root.join("link.txt").exists(),
                "nothing is published when the publication is refused"
            );
        }
    }

    #[test]
    fn recovery_never_settles_an_effect_the_caller_holds_no_evidence_about() {
        // The publication session is project-wide, so `pending` also returns
        // other agents' in-flight work. Settling someone else's record from
        // this caller's patch would write a terminal answer with no basis —
        // and could reconcile a live operation out from under the process
        // performing it.
        use crate::publication::{PublicationJournal, PublicationRequest, recover};
        use event_ledger::journal::OperationState;

        let repo = repo("publication-scope");
        let ledger_path = crate::interactive::project_ledger_path(
            &repo.root.join(crate::interactive::PROJECT_MARKER),
        );
        let journal = PublicationJournal::for_project(&ledger_path).expect("journal");

        let other = journal
            .journal()
            .prepare(
                journal.session(),
                &event_ledger::journal::EffectSpec::new(
                    crate::publication::PUBLISH_ACTION,
                    "another-agent",
                    "rev-other",
                    "base=other patch=other",
                )
                .expect("spec"),
                event_ledger::journal::IdempotencyClass::AtMostOnce,
                &Default::default(),
            )
            .expect("prepare");
        journal
            .journal()
            .mark_executing(other.id(), &Default::default())
            .expect("executing");

        // This caller recovers its OWN effect, which has nothing pending.
        let mine = PublicationRequest {
            principal: "me",
            parent_revision: "rev-mine",
            base_revision: "base-mine",
            patch: b"mine",
            change_count: 1,
        };
        let report = recover(&journal, mine.fingerprint().expect("fp"), |_| {
            panic!("no record of this effect exists, so nothing may be settled")
        })
        .expect("recover");
        assert_eq!(report.untouched, vec![other.id()]);
        assert!(report.reconciled.is_empty() && report.abandoned.is_empty());
        assert_eq!(
            journal
                .journal()
                .load(other.id(), &Default::default())
                .expect("load")
                .state(),
            OperationState::Executing,
            "another agent's live operation must be left exactly as it was"
        );
    }

    #[test]
    fn an_effect_reconciled_as_not_applied_is_never_reported_already_applied() {
        // `Reconciled` says the effect is settled, not that it landed. If a
        // not-applied reconciliation answered "already applied", integrate
        // would report success and then reset+clean the child's worktree —
        // destroying work that never reached the tree.
        use crate::publication::{
            PublicationJournal, PublicationRequest, Published, RECONCILED_NOT_APPLIED, publish,
        };

        let repo = repo("publication-not-applied");
        let ledger_path = crate::interactive::project_ledger_path(
            &repo.root.join(crate::interactive::PROJECT_MARKER),
        );
        let journal = PublicationJournal::for_project(&ledger_path).expect("journal");
        let request = PublicationRequest {
            principal: "agent",
            parent_revision: "rev-1",
            base_revision: "base-1",
            patch: b"patch",
            change_count: 1,
        };
        let spec = event_ledger::journal::EffectSpec::new(
            crate::publication::PUBLISH_ACTION,
            "agent",
            "rev-1",
            format!(
                "base=base-1 patch={}",
                protocol::ArtifactId::from_bytes(b"patch")
            ),
        )
        .expect("spec");
        let record = journal
            .journal()
            .prepare(
                journal.session(),
                &spec,
                event_ledger::journal::IdempotencyClass::AtMostOnce,
                &Default::default(),
            )
            .expect("prepare");
        journal
            .journal()
            .mark_executing(record.id(), &Default::default())
            .expect("executing");
        journal
            .journal()
            .mark_uncertain(record.id(), &Default::default())
            .expect("uncertain");
        journal
            .journal()
            .reconcile(record.id(), RECONCILED_NOT_APPLIED, &Default::default())
            .expect("reconciled as not applied");

        // The work still needs publishing, so the request must run.
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&ran);
        let outcome = publish(&journal, &request, move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .expect("publishes");
        assert!(
            matches!(outcome, Published::Applied(_)),
            "a not-applied reconciliation must not answer AlreadyApplied: {outcome:?}"
        );
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "the effect must actually run"
        );
    }

    #[test]
    fn a_prepared_publication_that_never_started_is_closed_so_a_retry_is_clean() {
        use crate::publication::{
            PublicationJournal, PublicationRequest, Published, Settlement, publish, recover,
        };

        let repo = repo("publication-prepared");
        let ledger_path = crate::interactive::project_ledger_path(
            &repo.root.join(crate::interactive::PROJECT_MARKER),
        );
        let journal = PublicationJournal::for_project(&ledger_path).expect("journal");
        journal
            .journal()
            .prepare(
                journal.session(),
                &event_ledger::journal::EffectSpec::new(
                    crate::publication::PUBLISH_ACTION,
                    "agent",
                    "rev-1",
                    format!(
                        "base=base-1 patch={}",
                        protocol::ArtifactId::from_bytes(b"abc")
                    ),
                )
                .expect("spec"),
                event_ledger::journal::IdempotencyClass::AtMostOnce,
                &Default::default(),
            )
            .expect("prepare");

        // Nothing started, so recovery closes it and a fresh request runs.
        let request = PublicationRequest {
            principal: "agent",
            parent_revision: "rev-1",
            base_revision: "base-1",
            patch: b"abc",
            change_count: 1,
        };
        let report = recover(&journal, request.fingerprint().expect("fp"), |_| {
            Settlement::Unknown
        })
        .expect("recover");
        assert_eq!(report.abandoned.len(), 1);
        assert!(report.reconciled.is_empty() && report.undecided.is_empty());

        assert!(matches!(
            publish(&journal, &request, || Ok(())).expect("runs after recovery"),
            Published::Applied(_)
        ));
    }

    #[test]
    fn integrating_a_child_stales_evidence_recorded_before_it() {
        // `git apply` writes the parent tree without going through
        // `workspace_write`'s hook, so before this an integrated patch left
        // every recorded check looking fresh against code it never saw.
        use agent_runtime::{
            Criterion, EvidenceRequirement, GoalActor, GoalBudget, GoalCommand, GoalSpec,
        };
        use protocol::GoalId;

        let repo = repo("publication-invalidates");
        let marker = repo.root.join(crate::interactive::PROJECT_MARKER);
        std::fs::create_dir_all(&marker).expect("marker");
        let evidence_path = marker.join(crate::goal_host::EVIDENCE_FILE);

        // A goal with one recorded, fresh piece of evidence.
        let mut host = crate::goal_host::GoalHost::new();
        let goal = GoalId::new();
        host.apply(
            GoalCommand::Create(
                GoalSpec::new(
                    goal,
                    "ship it",
                    vec![Criterion::new("c1", "it works").expect("criterion")],
                    GoalBudget::new(None, None, None, None),
                    vec![EvidenceRequirement::new("c1", vec!["test".to_owned()]).expect("req")],
                )
                .expect("spec"),
            ),
            &GoalActor::Human,
            &agent_runtime::CancellationToken::new(),
        )
        .expect("create");
        host.record_evidence(
            agent_runtime::EvidenceSpec::new(
                protocol::EvidenceId::new(),
                goal,
                agent_runtime::EvidenceKind::Test,
                agent_runtime::TEST_PASSED,
                agent_runtime::EvidenceProducer::System,
                agent_runtime::EvidenceSource::new(protocol::ArtifactId::from_bytes(b"out")),
                agent_runtime::EvidenceStatus::Passed,
                "child.txt",
            )
            .expect("spec")
            .with_command("cargo test")
            .expect("command"),
        )
        .expect("record");
        host.save_evidence(&evidence_path).expect("save");

        // Integrate a child: the tree changes.
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        std::fs::write(view.worktree.join("child.txt"), "from the child\n").unwrap();
        manager
            .integrate(&repo.root, agent, None)
            .expect("integrates");

        // The recorded evidence is no longer fresh.
        let mut reloaded = crate::goal_host::GoalHost::new();
        reloaded.load_evidence(&evidence_path).expect("reload");
        let stale = reloaded
            .evidence()
            .store()
            .records()
            .iter()
            .all(|record| !record.freshness().is_fresh());
        assert!(
            stale,
            "publication must stale evidence recorded before it: {:?}",
            reloaded
                .evidence()
                .store()
                .records()
                .iter()
                .map(|r| r.freshness())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_publication_that_cannot_be_journaled_does_not_happen() {
        // Fail closed: an at-most-once effect we cannot record is exactly
        // the one a crash would double-apply, so integration refuses rather
        // than writing the parent unrecorded.
        let repo = repo("publication-unjournalable");
        let manager = AgentViewManager::new();
        let agent = AgentId::new();
        let view = manager.create_for(&repo.root, agent).expect("view");
        std::fs::write(view.worktree.join("child.txt"), "from the child\n").unwrap();
        // The ledger path is occupied by a directory, so it cannot be opened.
        let ledger_path = crate::interactive::project_ledger_path(
            &repo.root.join(crate::interactive::PROJECT_MARKER),
        );
        let _ = std::fs::remove_file(&ledger_path);
        std::fs::create_dir_all(&ledger_path).expect("occupy the ledger path");

        let err = manager
            .integrate(&repo.root, agent, None)
            .expect_err("refuses without a journal");
        assert!(err.contains("journal"), "{err}");
        assert!(
            !repo.root.join("child.txt").exists(),
            "the parent tree must be untouched when the effect cannot be recorded"
        );
    }

    #[test]
    fn a_failed_publication_is_journaled_failed_and_leaves_the_tree_alone() {
        use crate::publication::{
            PublicationError, PublicationJournal, PublicationRequest, publish,
        };
        use event_ledger::journal::OperationState;

        let repo = repo("publication-failed");
        let ledger_path = crate::interactive::project_ledger_path(
            &repo.root.join(crate::interactive::PROJECT_MARKER),
        );
        let journal = PublicationJournal::for_project(&ledger_path).expect("journal");
        let request = PublicationRequest {
            principal: "agent",
            parent_revision: "abc123",
            base_revision: "def456",
            patch: b"patch",
            change_count: 2,
        };
        let err = publish(&journal, &request, || {
            Err(crate::publication::EffectFailure::Untouched(
                "the apply failed".to_owned(),
            ))
        })
        .expect_err("propagates the failure");
        assert!(matches!(err, PublicationError::Failed(ref d) if d == "the apply failed"));

        // Recorded as Failed — so recovery has nothing to reconcile — and a
        // retry of the same effect is allowed to run.
        let record = journal
            .journal()
            .find_by_fingerprint(
                journal.session(),
                event_ledger::journal::EffectFingerprint::compute(
                    &event_ledger::journal::EffectSpec::new(
                        crate::publication::PUBLISH_ACTION,
                        "agent",
                        "abc123",
                        format!(
                            "base=def456 patch={}",
                            protocol::ArtifactId::from_bytes(b"patch")
                        ),
                    )
                    .expect("spec"),
                ),
                &Default::default(),
            )
            .expect("lookup")
            .expect("a record exists");
        assert_eq!(record.state(), OperationState::Failed);
        assert!(
            publish(&journal, &request, || Ok(())).is_ok(),
            "a failed effect may be retried"
        );
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
