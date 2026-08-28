//! Model-callable coding tools for the `exec` subcommand, gated on project
//! trust (fail-closed): a bounded file write, a bounded file read, paginated
//! `repo_read`, workspace `repo_search`, exact-match `workspace_patch`, and
//! supervised `shell_exec`. Paths are relative and must resolve strictly
//! inside the trusted workspace root — absolute paths, `..` components, and
//! symlink escapes are refused, and all tool output is byte-capped.
//!
//! One model step's calls dispatch as a batch: read-classified calls run
//! concurrently, write-classified calls serialize per target (all
//! `shell_exec` calls serialize with each other), and results keep their
//! per-call ids and outcomes. Every call passes the six-mode permission
//! lattice before execution; a headless denial is a typed model-visible
//! result, never a silent pass.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_runtime::{
    CancellationToken, ProposedToolCall, ToolDriver, ToolKind, ToolStepError, ToolStepResult,
    ToolSurface, ValidatedToolCall,
};

use crate::permissions::{Decision, PermissionLattice, PermissionMode, ToolClass};

/// Tool name for a bounded workspace file write.
pub const WORKSPACE_WRITE_TOOL: &str = "workspace_write";
/// Tool name for a bounded workspace file read.
pub const WORKSPACE_READ_TOOL: &str = "workspace_read";
/// Tool name for a paginated bounded file read (gateway name `repo_read`).
pub const REPO_READ_TOOL: &str = "repo_read";
/// Tool name for a bounded workspace text search (gateway name `repo_search`).
pub const REPO_SEARCH_TOOL: &str = "repo_search";
/// Tool name for an exact-match edit (gateway name `workspace_patch`).
pub const WORKSPACE_PATCH_TOOL: &str = "workspace_patch";
/// Tool name for supervised command execution (gateway name `shell_exec`).
pub const SHELL_EXEC_TOOL: &str = "shell_exec";
/// Tool name for file-pattern search (Claude `Glob` parity).
pub const REPO_GLOB_TOOL: &str = "repo_glob";
/// Tool name for the model-callable task list (Claude `TodoWrite` parity).
pub const TODO_WRITE_TOOL: &str = "todo_write";
/// Hard cap on `repo_glob` results (Claude truncates Glob at 100 files).
pub const MAX_GLOB_RESULTS: usize = 100;
/// Hard cap on one task-list entry.
pub const MAX_TODO_CONTENT_BYTES: usize = 512;
/// Hard cap on retained task-list entries.
pub const MAX_TODOS: usize = 50;
/// Workspace-relative path of the persisted task list.
pub const TODOS_PATH: &str = ".rapidlm/todos.json";
/// Tool name for entering plan mode (Claude `EnterPlanMode` parity).
pub const PLAN_ENTER_TOOL: &str = "plan_enter";
/// Tool name for exiting plan mode with the written plan (Claude `ExitPlanMode`).
pub const PLAN_EXIT_TOOL: &str = "plan_exit";
/// Tool name for spawning a subagent (Claude `Agent`/`Task` parity).
pub const TASK_SPAWN_TOOL: &str = "task_spawn";
/// Tool name for background job status (Claude `TaskOutput`/`TaskStop` parity).
pub const JOB_STATUS_TOOL: &str = "job_status";
/// Tool name for reading background job output.
pub const JOB_OUTPUT_TOOL: &str = "job_output";
/// Workspace-relative path of the plan file (the only writable file in plan
/// mode — Claude's plan-file carve-out).
pub const PLAN_PATH: &str = ".rapidlm/plan.md";
/// Maximum live background jobs per run.
pub const MAX_BACKGROUND_JOBS: usize = 16;
/// Poll interval for background job supervision.
pub const JOB_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Hard byte cap on one background job's spooled output.
pub const MAX_JOB_OUTPUT_BYTES: usize = 64 * 1024;
/// Default wall-clock budget for one background job.
pub const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(600);
/// Hard byte cap on one subagent prompt.
pub const MAX_SPAWN_PROMPT_BYTES: usize = 16 * 1024;
/// Hard byte cap on one subagent's returned report.
pub const MAX_SUBAGENT_REPORT_BYTES: usize = 16 * 1024;
/// Hard byte cap for the plan file.
pub const MAX_PLAN_BYTES: usize = 16 * 1024;
/// Adopted subagent types (the names both reference CLIs standardized on).
pub const AGENT_TYPES: &[&str] = &["general-purpose", "explore", "plan"];
/// Hard byte cap on one tool call's JSON arguments.
pub const MAX_TOOL_ARGUMENTS_BYTES: usize = 8 * 1024;
/// Hard byte cap on a relative workspace path.
pub const MAX_TOOL_PATH_BYTES: usize = 512;
/// Hard byte cap on one file write.
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
/// Hard byte cap on one file read returned to the model.
pub const MAX_READ_BYTES: usize = 4 * 1024;
/// Default 1-indexed start line for `repo_read`.
pub const DEFAULT_READ_OFFSET: usize = 1;
/// Default line window for `repo_read`.
pub const DEFAULT_READ_LINES: usize = 200;
/// Hard ceiling on the `repo_read` line window.
pub const MAX_READ_LINES: usize = 1_000;
/// Default hit cap for `repo_search`.
pub const DEFAULT_SEARCH_HEAD_LIMIT: usize = 20;
/// Hard ceiling on the `repo_search` hit cap.
pub const MAX_SEARCH_HEAD_LIMIT: usize = 100;
/// Maximum files one `repo_search` may walk.
pub const MAX_SEARCH_FILES: usize = 2_000;
/// Hard byte cap on one `repo_search` result payload.
pub const MAX_SEARCH_OUTPUT_BYTES: usize = 8 * 1024;
/// Hard byte cap on one `workspace_patch` old/new text. Two texts plus the
/// path must fit the per-call argument payload bound (8 KiB), so each text is
/// capped at 3 KiB.
pub const MAX_PATCH_TEXT_BYTES: usize = 3 * 1024;
/// Hard byte cap on one `shell_exec` argv.
pub const MAX_SHELL_ARGV: usize = 64;
/// Hard byte cap on one `shell_exec` argv token.
pub const MAX_SHELL_ARG_BYTES: usize = 4 * 1024;
/// Default wall-clock budget for one `shell_exec`.
pub const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum wall-clock budget for one `shell_exec`.
pub const MAX_SHELL_TIMEOUT: Duration = Duration::from_secs(600);
/// Hard byte cap on captured `shell_exec` output.
pub const MAX_SHELL_OUTPUT_BYTES: usize = 16 * 1024;
/// Hard byte cap on model-visible per-call denial/failure detail text.
pub const MAX_RESULT_DETAIL_BYTES: usize = 256;
/// Marker appended when output was cut by a byte cap.
pub const TRUNCATION_MARKER: &str = "\n[truncated]";
/// Directories `repo_search` never descends into.
const SEARCH_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".rapidlm"];

/// Typed workspace-tools setup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolSetupError {
    /// The workspace root does not exist or is not a directory.
    RootNotADirectory,
    /// The workspace root could not be canonicalized.
    RootUnresolvable,
}

impl std::fmt::Display for ToolSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RootNotADirectory => "workspace root is not a directory",
            Self::RootUnresolvable => "workspace root could not be resolved",
        })
    }
}

/// A background command: a spooled output buffer, a cancel flag the owner
/// sets on shutdown, and the terminal state. All shared state sits behind
/// mutexes/atomics so the supervising thread and tool calls never block each
/// other for long.
#[derive(Clone)]
struct JobShared {
    cancelled: Arc<AtomicBool>,
    output: Arc<Mutex<Vec<u8>>>,
    overflow: Arc<AtomicBool>,
    state: Arc<Mutex<JobState>>,
    child: Arc<Mutex<Option<std::process::Child>>>,
}

enum JobState {
    Running,
    Completed(i32),
    Failed(String),
}

impl JobState {
    fn as_text(&self) -> String {
        match self {
            Self::Running => "running".to_owned(),
            Self::Completed(code) => format!("completed exit {code}"),
            Self::Failed(reason) => format!("failed: {reason}"),
        }
    }
}

/// Registry of background commands started by `shell.exec` with
/// `background: true`. Children are killed when the registry drops, so no
/// command outlives the CLI run.
#[derive(Clone, Default)]
pub struct JobRegistry {
    jobs: Arc<Mutex<BTreeMap<String, JobShared>>>,
    seq: Arc<AtomicU64>,
}

impl JobRegistry {
    /// Start `argv` in `cwd` as a detached supervised job; returns its id.
    /// The supervisor thread enforces the timeout, honors cancellation, spools
    /// combined output up to [`MAX_JOB_OUTPUT_BYTES`], and records the exit.
    fn start(
        &self,
        argv: &[String],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<String, ToolStepError> {
        {
            let jobs = self.jobs.lock().map_err(|_| ToolStepError::Failed)?;
            let live = jobs
                .values()
                .filter(|job| {
                    job.state
                        .lock()
                        .map(|state| matches!(*state, JobState::Running))
                        .unwrap_or(false)
                })
                .count();
            if live >= MAX_BACKGROUND_JOBS {
                return Err(ToolStepError::Failed);
            }
        }
        let id = format!("job-{}", self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        let shared = JobShared {
            cancelled: Arc::new(AtomicBool::new(false)),
            output: Arc::new(Mutex::new(Vec::new())),
            overflow: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(JobState::Running)),
            child: Arc::new(Mutex::new(None)),
        };
        self.jobs
            .lock()
            .map_err(|_| ToolStepError::Failed)?
            .insert(id.clone(), shared.clone());

        // Everything the supervisor touches is owned and 'static: the job
        // must outlive the tool call (and even a batch dispatch thread).
        let program = argv[0].clone();
        let rest: Vec<String> = argv[1..].to_vec();
        let dir = cwd.to_path_buf();
        let env_pairs: Vec<(String, String)> = ["PATH", "HOME", "LANG", "TMPDIR"]
            .iter()
            .filter_map(|key| {
                std::env::var(key)
                    .ok()
                    .map(|value| ((*key).to_owned(), value))
            })
            .collect();
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("rapidlm-job".to_owned())
            .spawn(move || {
                let started = Instant::now();
                // Spawn under the child lock (scoped: the guard must drop
                // before the loop re-locks to publish the child).
                let spawned_child = {
                    let mut slot = worker.child.lock().ok();
                    slot.as_mut().and_then(|_slot| {
                        let mut command = std::process::Command::new(&program);
                        command
                            .args(&rest)
                            .current_dir(&dir)
                            .env_clear()
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped());
                        for (key, value) in &env_pairs {
                            let _ = command.env(key, value);
                        }
                        command.spawn().ok()
                    })
                };
                let mut child = match spawned_child {
                    Some(child) => child,
                    None => {
                        if let Ok(mut state) = worker.state.lock() {
                            *state = JobState::Failed("spawn failed".to_owned());
                        }
                        return;
                    }
                };
                // Take the pipes first, then publish the child so kill-all and
                // the supervision loop can see it.
                let pipes: Vec<Box<dyn Read + Send>> = vec![
                    Box::new(child.stdout.take().expect("stdout piped")),
                    Box::new(child.stderr.take().expect("stderr piped")),
                ];
                if let Ok(mut slot) = worker.child.lock() {
                    *slot = Some(child);
                }
                let mut readers = Vec::new();
                for pipe in pipes {
                    let output = Arc::clone(&worker.output);
                    let overflow = Arc::clone(&worker.overflow);
                    readers.push(std::thread::spawn(move || {
                        let mut pipe = pipe;
                        let mut chunk = [0u8; 2048];
                        loop {
                            match pipe.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    let Ok(mut spool) = output.lock() else {
                                        return;
                                    };
                                    if spool.len() >= MAX_JOB_OUTPUT_BYTES {
                                        overflow.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                    let take = n.min(MAX_JOB_OUTPUT_BYTES - spool.len());
                                    spool.extend_from_slice(&chunk[..take]);
                                    if take < n {
                                        overflow.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            }
                        }
                    }));
                }
                // Supervise: exit, cancellation, or timeout — whichever first.
                loop {
                    // Scope the lock guard: try_wait borrows the slot.
                    let done = {
                        let mut slot = worker.child.lock().ok();
                        slot.as_mut()
                            .and_then(|child| child.as_mut())
                            .and_then(|child| child.try_wait().ok())
                            .flatten()
                    };
                    if let Some(status) = done {
                        if let Ok(mut state) = worker.state.lock() {
                            *state = JobState::Completed(status.code().unwrap_or(-1));
                        }
                        break;
                    }
                    if worker.cancelled.load(Ordering::SeqCst) {
                        if let Ok(mut slot) = worker.child.lock() {
                            if let Some(child) = slot.as_mut() {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
                        }
                        if let Ok(mut state) = worker.state.lock() {
                            *state = JobState::Failed("cancelled at shutdown".to_owned());
                        }
                        break;
                    }
                    if started.elapsed() > timeout {
                        if let Ok(mut slot) = worker.child.lock() {
                            if let Some(child) = slot.as_mut() {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
                        }
                        if let Ok(mut state) = worker.state.lock() {
                            *state = JobState::Failed("timed out".to_owned());
                        }
                        break;
                    }
                    std::thread::sleep(JOB_POLL_INTERVAL);
                }
                for reader in readers {
                    let _ = reader.join();
                }
            });
        if spawned.is_err() {
            // The supervisor thread could not start; retract the job.
            if let Ok(mut jobs) = self.jobs.lock() {
                jobs.remove(&id);
            }
            return Err(ToolStepError::Failed);
        }
        self.prune();
        Ok(id)
    }

    fn snapshot(&self, id: &str) -> Option<String> {
        let jobs = self.jobs.lock().ok()?;
        jobs.get(id)
            .map(|job| job.state.lock().ok().map(|state| state.as_text()))
            .flatten()
    }

    /// Bounded slice of spooled output starting at `offset`; returns the text,
    /// whether the job is finished, and the next offset to read from.
    fn output(&self, id: &str, offset: usize) -> Option<(String, bool, usize, String)> {
        let jobs = self.jobs.lock().ok()?;
        let job = jobs.get(id)?;
        let buffer = job.output.lock().ok()?;
        let start = offset.min(buffer.len());
        let end = (start + MAX_SHELL_OUTPUT_BYTES).min(buffer.len());
        let text = String::from_utf8_lossy(&buffer[start..end]).into_owned();
        let next = start + (end - start);
        let state = job.state.lock().ok()?;
        let done = !matches!(*state, JobState::Running);
        Some((text, done, next, state.as_text()))
    }

    fn kill_all(&self) {
        let Ok(jobs) = self.jobs.lock() else {
            return;
        };
        for job in jobs.values() {
            job.cancelled.store(true, Ordering::SeqCst);
            if let Ok(mut child) = job.child.try_lock() {
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                }
            }
        }
    }

    /// Retain only live jobs once finished ones exceed the registry bound.
    fn prune(&self) {
        let Ok(mut jobs) = self.jobs.lock() else {
            return;
        };
        let finished: Vec<String> = jobs
            .iter()
            .filter(|(_, job)| {
                job.state
                    .lock()
                    .map(|state| !matches!(*state, JobState::Running))
                    .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        let excess = jobs.len().saturating_sub(MAX_BACKGROUND_JOBS);
        for id in finished.into_iter().take(excess) {
            jobs.remove(&id);
        }
    }
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// Subagent execution seam: `task_spawn` hands the prompt to this runner,
/// which owns the child model and a depth-restricted tool surface. Adopted
/// from the reference CLIs: types are general-purpose | explore | plan, and
/// children never get the spawn tool (depth limit 1).
pub trait SubagentRunner: Send + Sync {
    fn run(&self, prompt: &str, agent_type: &str) -> Result<String, String>;
}

/// Bounded tools rooted at one canonical workspace directory, with the
/// permission lattice that gates every call.
pub struct WorkspaceTools {
    root: PathBuf,
    permissions: PermissionLattice,
    jobs: JobRegistry,
    plan_mode: Arc<AtomicBool>,
    read_only: bool,
    subagents: Option<Arc<dyn SubagentRunner>>,
}

impl WorkspaceTools {
    /// Bind the tools to a workspace root. The root is canonicalized once so
    /// every containment check compares against the real directory.
    pub fn open(root: &Path) -> Result<Self, ToolSetupError> {
        Self::open_with_permissions(root, PermissionLattice::new(PermissionMode::Default))
    }

    /// Bind the tools with an explicit permission lattice.
    pub fn open_with_permissions(
        root: &Path,
        permissions: PermissionLattice,
    ) -> Result<Self, ToolSetupError> {
        if !root.is_dir() {
            return Err(ToolSetupError::RootNotADirectory);
        }
        let root = root.canonicalize().map_err(|_| ToolSetupError::RootUnresolvable)?;
        Ok(Self {
            root,
            permissions,
            jobs: JobRegistry::default(),
            plan_mode: Arc::new(AtomicBool::new(false)),
            read_only: false,
            subagents: None,
        })
    }

    /// Read-only driver for subagent explore/plan scopes: write-classified
    /// tools are not advertised and any write attempt is refused.
    pub fn open_read_only(root: &Path) -> Result<Self, ToolSetupError> {
        let mut tools = Self::open(root)?;
        tools.read_only = true;
        Ok(tools)
    }

    /// Attach the subagent runner (composition root only; children are built
    /// without one, which enforces the depth limit structurally).
    pub fn set_subagent_runner(&mut self, runner: Arc<dyn SubagentRunner>) {
        self.subagents = Some(runner);
    }

    fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a checked relative path inside the root. The containing
    /// directory is created if missing and canonicalized, so a symlinked
    /// directory cannot move the target outside the workspace.
    fn resolve_in_root(&self, relative: &str) -> Result<PathBuf, ToolStepError> {
        let target = self.root.join(checked_relative(relative)?);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
            let resolved = parent.canonicalize().map_err(|_| ToolStepError::Invalid)?;
            if !resolved.starts_with(self.root()) {
                return Err(ToolStepError::Invalid);
            }
        }
        Ok(target)
    }

    /// Rule-matching subject for one call: the workspace-relative path for
    /// file tools, the joined argv for `shell_exec`.
    fn rule_subject(tool: &str, arguments: &str) -> Option<String> {
        match tool {
            WORKSPACE_WRITE_TOOL => parse_write_args(arguments).ok().map(|args| args.path),
            WORKSPACE_READ_TOOL | REPO_READ_TOOL => parse_path_argument(arguments),
            WORKSPACE_PATCH_TOOL => parse_patch_args(arguments).ok().map(|args| args.path),
            SHELL_EXEC_TOOL => parse_shell_args(arguments).ok().map(|args| args.argv.join(" ")),
            REPO_GLOB_TOOL => parse_repo_glob_args(arguments).ok().map(|args| args.pattern),
            TODO_WRITE_TOOL => Some(TODOS_PATH.to_owned()),
            PLAN_ENTER_TOOL => Some(PLAN_ENTER_TOOL.to_owned()),
            PLAN_EXIT_TOOL => Some(PLAN_EXIT_TOOL.to_owned()),
            TASK_SPAWN_TOOL => Some(TASK_SPAWN_TOOL.to_owned()),
            _ => None,
        }
    }

    /// Permission decision for one validated call. Total: every call of a
    /// known tool gets a decision. While plan mode is active the decision is
    /// additionally gated: only read-only calls and writes to the plan file
    /// pass (Claude's plan-file carve-out).
    fn permission_for(&self, call: &ValidatedToolCall) -> Decision {
        let subject =
            Self::rule_subject(call.tool(), call.arguments()).unwrap_or_default();
        let decision = self
            .permissions
            .evaluate(call.tool(), &subject, tool_class(call.tool()));
        if !decision.is_allowed() {
            return decision;
        }
        if self.plan_mode.load(Ordering::SeqCst)
            && tool_class(call.tool()) != ToolClass::ReadOnly
            && !matches!(call.tool(), PLAN_ENTER_TOOL | PLAN_EXIT_TOOL)
            && subject != PLAN_PATH
        {
            return Decision::Deny(crate::permissions::DecisionReason::PlanModeDeny);
        }
        decision
    }

    /// Execute one validated call against the workspace. Inherent `&self` so
    /// the batch dispatcher can run independent calls on threads.
    fn execute_call(
        &self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        cancel.check().map_err(|_| ToolStepError::Cancelled)?;
        // Permission gate: deny and headless-ask are typed model-visible
        // denials, never silent passes.
        let decision = self.permission_for(call);
        if !decision.is_allowed() {
            return Ok(ToolStepResult::Denied {
                call_id: call.call_id().to_owned(),
                detail: Some(bounded_detail(&format!(
                    "{} denied: {}",
                    call.tool(),
                    decision.reason().explanation()
                ))),
            });
        }
        // Argument re-validation: a known tool with malformed or oversized
        // arguments is a per-call handled failure the model can correct —
        // never a dead turn. Unknown tools stay structural refusals.
        if call.arguments().len() > MAX_TOOL_ARGUMENTS_BYTES {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "arguments exceed the {MAX_TOOL_ARGUMENTS_BYTES}-byte bound"
                ))),
            });
        }
        if self.read_only && tool_kind(call.tool()) == ToolKind::Write {
            return Ok(ToolStepResult::Denied {
                call_id: call.call_id().to_owned(),
                detail: Some(bounded_detail(
                    "this subagent scope is read-only; write tools are unavailable",
                )),
            });
        }
        let arguments_parseable = match call.tool() {
            WORKSPACE_WRITE_TOOL => parse_write_args(call.arguments()).is_ok(),
            WORKSPACE_READ_TOOL => parse_path_argument(call.arguments()).is_some(),
            REPO_READ_TOOL => parse_repo_read_args(call.arguments()).is_ok(),
            REPO_SEARCH_TOOL => parse_repo_search_args(call.arguments()).is_ok(),
            WORKSPACE_PATCH_TOOL => parse_patch_args(call.arguments()).is_ok(),
            SHELL_EXEC_TOOL => parse_shell_args(call.arguments()).is_ok(),
            REPO_GLOB_TOOL => parse_repo_glob_args(call.arguments()).is_ok(),
            TODO_WRITE_TOOL => parse_todo_args(call.arguments()).is_ok(),
            PLAN_ENTER_TOOL | PLAN_EXIT_TOOL => parse_empty_args(call.arguments()).is_ok(),
            JOB_STATUS_TOOL => parse_job_id_args(call.arguments(), false).is_ok(),
            JOB_OUTPUT_TOOL => parse_job_id_args(call.arguments(), true).is_ok(),
            TASK_SPAWN_TOOL => parse_task_args(call.arguments()).is_ok(),
            _ => return Err(ToolStepError::Invalid),
        };
        if !arguments_parseable {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "invalid arguments for {} (JSON with the documented fields and bounds)",
                    call.tool()
                ))),
            });
        }
        match call.tool() {
            WORKSPACE_WRITE_TOOL => self.execute_write(call, cancel),
            WORKSPACE_READ_TOOL => self.execute_read(call, cancel),
            REPO_READ_TOOL => self.execute_repo_read(call, cancel),
            REPO_SEARCH_TOOL => self.execute_repo_search(call, cancel),
            WORKSPACE_PATCH_TOOL => self.execute_patch(call, cancel),
            SHELL_EXEC_TOOL => self.execute_shell(call, cancel),
            REPO_GLOB_TOOL => self.execute_repo_glob(call, cancel),
            TODO_WRITE_TOOL => self.execute_todo_write(call, cancel),
            PLAN_ENTER_TOOL => self.execute_plan_enter(call, cancel),
            PLAN_EXIT_TOOL => self.execute_plan_exit(call, cancel),
            JOB_STATUS_TOOL => self.execute_job_status(call, cancel),
            JOB_OUTPUT_TOOL => self.execute_job_output(call, cancel),
            TASK_SPAWN_TOOL => self.execute_task_spawn(call, cancel),
            _ => Err(ToolStepError::Invalid),
        }
    }

    fn execute_write(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_write_args(call.arguments())?;
        let target = self.resolve_in_root(&args.path)?;
        fs::write(&target, args.content.as_bytes()).map_err(|_| ToolStepError::Failed)?;
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary: format!("wrote {} bytes to {}", args.content.len(), args.path),
        })
    }

    fn execute_read(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_path_argument(call.arguments()).ok_or(ToolStepError::Invalid)?;
        let target = self.resolve_in_root(&args)?;
        match fs::read(&target) {
            Ok(bytes) => Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: bounded_text(&bytes, MAX_READ_BYTES),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Model-visible, handled failure: the turn continues
                // and the model can correct the path.
                Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&format!("{args}: file not found"))),
                })
            }
            Err(_) => Err(ToolStepError::Failed),
        }
    }

    /// `repo_read`: bounded line window `[offset, offset+limit)` of a
    /// workspace-relative text file, byte-capped, with a truncation marker.
    fn execute_repo_read(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_repo_read_args(call.arguments())?;
        let target = self.resolve_in_root(&args.path)?;
        let bytes = match fs::read(&target) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&format!("{}: file not found", args.path))),
                })
            }
            Err(_) => return Err(ToolStepError::Failed),
        };
        let text = String::from_utf8_lossy(&bytes);
        let line_count = text.lines().count();
        let start = args.offset.saturating_sub(1).min(line_count);
        let end = start.saturating_add(args.limit).min(line_count);
        let window: Vec<&str> = text.lines().skip(start).take(end - start).collect();
        let mut summary = bounded_text(window.join("\n").as_bytes(), MAX_READ_BYTES);
        // When the byte cap cut the page, report the window actually
        // delivered — the model must never be told it has lines it cannot
        // see — plus the line to continue from.
        if summary.contains(TRUNCATION_MARKER) {
            let delivered_text = summary.strip_suffix(TRUNCATION_MARKER).unwrap_or("");
            let delivered = if delivered_text.is_empty() {
                0
            } else {
                delivered_text.matches('\n').count() + 1
            };
            let delivered_end = start + delivered;
            summary.push_str(&format!(
                " (byte cap: lines {}-{} of {} delivered; continue at {})",
                start + 1,
                delivered_end,
                line_count,
                delivered_end + 1
            ));
        } else if end < line_count {
            summary.push_str(&format!(
                "{TRUNCATION_MARKER} (lines {}-{} of {})",
                start + 1,
                end,
                line_count
            ));
        } else if end - start < args.limit {
            // The window ended before its limit: the file was fully consumed.
            summary.push_str(&format!(
                "\n[end of file: lines {}-{} of {}]",
                start + 1,
                end,
                line_count
            ));
        }
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary,
        })
    }

    /// `repo_search`: bounded exact-substring search over workspace text
    /// files with `head_limit`/`offset` pagination over the hit list.
    fn execute_repo_search(
        &self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_repo_search_args(call.arguments())?;
        let mut all_hits: Vec<String> = Vec::new();
        let mut walked = 0usize;
        walk_text_files(self.root(), self.root(), 0, &mut walked, &mut |path, contents| {
            if cancel.is_cancelled() {
                return;
            }
            for (index, line) in contents.lines().enumerate() {
                if !line.contains(&args.pattern) {
                    continue;
                }
                let line_text: String = if line.chars().count() > 200 {
                    let cut: String = line.chars().take(200).collect();
                    format!("{cut}…")
                } else {
                    line.to_owned()
                };
                all_hits.push(format!("{path}:{}: {line_text}", index + 1));
                return; // one hit per file keeps the result compact
            }
        });
        if cancel.is_cancelled() {
            return Err(ToolStepError::Cancelled);
        }
        let total = all_hits.len();
        let page: Vec<&str> = all_hits
            .iter()
            .skip(args.offset)
            .take(args.head_limit)
            .map(|hit| hit.as_str())
            .collect();
        if page.is_empty() {
            return Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: format!(
                    "no matches for {:?} (hits beyond offset skipped: {})",
                    args.pattern, total
                ),
            });
        }
        let mut summary = bounded_text(page.join("\n").as_bytes(), MAX_SEARCH_OUTPUT_BYTES);
        if summary.ends_with(TRUNCATION_MARKER) {
            summary.push_str(&format!(" (showing first page of {total} hits)"));
        } else if args.offset + page.len() < total {
            summary.push_str(&format!(
                "\n[more hits: {}/{}]",
                total - args.offset - page.len(),
                total
            ));
        }
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary,
        })
    }

    /// `workspace_patch`: exact-match replacement; `old` must appear exactly
    /// once unless `replace_all`, and must differ from `new`.
    fn execute_patch(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_patch_args(call.arguments())?;
        let target = self.resolve_in_root(&args.path)?;
        let bytes = match fs::read(&target) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&format!("{}: file not found", args.path))),
                })
            }
            Err(_) => return Err(ToolStepError::Failed),
        };
        let contents = String::from_utf8(bytes).map_err(|_| ToolStepError::Invalid)?;
        let occurrences = contents.matches(&args.old).count();
        if occurrences == 0 {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "{}: old text not found",
                    args.path
                ))),
            });
        }
        if occurrences > 1 && !args.replace_all {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "{}: old text matches {occurrences} locations; expand old text or set replace_all",
                    args.path
                ))),
            });
        }
        let updated = contents.replace(&args.old, &args.new);
        fs::write(&target, updated.as_bytes()).map_err(|_| ToolStepError::Failed)?;
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary: format!("replaced {occurrences} occurrence(s) in {}", args.path),
        })
    }

    /// `shell_exec`: supervised argv execution inside the workspace root with
    /// a wall-clock timeout, an allowlisted environment, empty stdin, and
    /// byte-captured combined output. No shell string is ever interpreted.
    fn execute_shell(
        &self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_shell_args(call.arguments())?;
        if args.background {
            let job_id = self.jobs.start(&args.argv, self.root(), args.timeout)?;
            return Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: format!(
                    "started background job {job_id}: {} (timeout {}s); poll with job_status / read with job_output",
                    args.argv.join(" "),
                    args.timeout.as_secs()
                ),
            });
        }
        let mut command = std::process::Command::new(&args.argv[0]);
        command
            .args(&args.argv[1..])
            .current_dir(self.root())
            .env_clear();
        for key in ["PATH", "HOME", "LANG", "TMPDIR"] {
            if let Ok(value) = std::env::var(key) {
                let _ = command.env(key, value);
            }
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|_| ToolStepError::Failed)?;
        let deadline = Instant::now() + args.timeout;
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break Ok(status);
            }
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                break Err(ToolStepError::Cancelled);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break Err(ToolStepError::Failed);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let output_text = match child.wait_with_output() {
            Ok(output) => {
                let mut combined =
                    Vec::with_capacity(output.stdout.len() + output.stderr.len());
                combined.extend_from_slice(&output.stdout);
                combined.extend_from_slice(&output.stderr);
                bounded_text(&combined, MAX_SHELL_OUTPUT_BYTES)
            }
            Err(_) => String::new(),
        };
        match status {
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary: format!("exit {code}\n{output_text}"),
                })
            }
            Err(ToolStepError::Cancelled) => Err(ToolStepError::Cancelled),
            Err(_) => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "timed out after {}ms\n{output_text}",
                    args.timeout.as_millis()
                ))),
            }),
        }
    }

    /// `repo_glob`: file-pattern search over workspace paths with `**` /
    /// `*` / `?` semantics (Claude `Glob` parity), capped results.
    fn execute_repo_glob(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_repo_glob_args(call.arguments())?;
        let mut matches: Vec<String> = Vec::new();
        let mut walked = 0usize;
        let mut total = 0usize;
        walk_all_files(self.root(), self.root(), 0, &mut walked, &mut |relative| {
            if glob_path_match(&args.pattern, relative) {
                total += 1;
                if matches.len() < args.head_limit {
                    matches.push(relative.to_owned());
                }
            }
        });
        if matches.is_empty() {
            return Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: format!("no files match {:?}", args.pattern),
            });
        }
        let mut summary = matches.join("\n");
        if total > matches.len() {
            summary.push_str(&format!(
                "{TRUNCATION_MARKER} (showing {} of {total} matches)",
                matches.len()
            ));
        }
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary,
        })
    }

    /// `todo_write`: merge-by-id model task list (Claude `TodoWrite` parity),
    /// persisted to `.rapidlm/todos.json` so the list survives across turns.
    fn execute_todo_write(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_todo_args(call.arguments())?;
        let existing = self.load_todos();
        let mut todos = existing.clone();
        for entry in &args.todos {
            match entry.id.as_deref() {
                Some(id) => {
                    if let Some(slot) = todos.iter_mut().find(|todo| todo.id.as_deref() == Some(id)) {
                        slot.content = entry.content.clone();
                        slot.status = entry.status.clone();
                    } else {
                        todos.push(TodoEntry {
                            id: Some(id.to_owned()),
                            content: entry.content.clone(),
                            status: entry.status.clone(),
                        });
                    }
                }
                None => {
                    // Id-less entries are appended with the next free numeric
                    // id so later writes can address them by id.
                    let mut next = 1usize;
                    while todos
                        .iter()
                        .any(|todo| todo.id.as_deref() == Some(next.to_string().as_str()))
                    {
                        next += 1;
                    }
                    todos.push(TodoEntry {
                        id: Some(next.to_string()),
                        content: entry.content.clone(),
                        status: entry.status.clone(),
                    });
                }
            }
        }
        if todos.len() > MAX_TODOS {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "task list exceeds {MAX_TODOS} entries; mark old tasks completed first"
                ))),
            });
        }
        let target = self.resolve_in_root(TODOS_PATH)?;
        let document = serde_json::json!({
            "schema": 1,
            "todos": todos.iter().map(|todo| serde_json::json!({
                "id": todo.id,
                "content": todo.content,
                "status": todo.status,
            })).collect::<Vec<_>>(),
        });
        fs::write(
            &target,
            serde_json::to_vec_pretty(&document).map_err(|_| ToolStepError::Failed)?,
        )
        .map_err(|_| ToolStepError::Failed)?;
        let count = |status: &str| {
            todos
                .iter()
                .filter(|todo| todo.status == status)
                .count()
        };
        let mut summary = format!(
            "{} task(s): {} pending, {} in_progress, {} completed",
            todos.len(),
            count("pending"),
            count("in_progress"),
            count("completed")
        );
        for todo in todos.iter().filter(|todo| todo.status == "in_progress") {
            summary.push_str(&format!("\n→ {}", todo.content));
        }
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary,
        })
    }

    fn load_todos(&self) -> Vec<TodoEntry> {
        let Ok(target) = self.resolve_in_root(TODOS_PATH) else {
            return Vec::new();
        };
        let Ok(bytes) = fs::read(&target) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Vec::new();
        };
        let Some(entries) = value.get("todos").and_then(serde_json::Value::as_array) else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_owned();
                let content = entry.get("content")?.as_str()?.to_owned();
                let status = entry.get("status")?.as_str()?.to_owned();
                Some(TodoEntry {
                    id: Some(id),
                    content,
                    status,
                })
            })
            .collect()
    }

    /// `plan_enter`: activate read-only enforcement with the plan-file
    /// carve-out (Claude `EnterPlanMode` parity).
    fn execute_plan_enter(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        self.plan_mode.store(true, Ordering::SeqCst);
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary: format!(
                "Plan mode active: only read-only calls and writes to {PLAN_PATH} are                  allowed. Research, write the plan to {PLAN_PATH}, then call plan_exit."
            ),
        })
    }

    /// `plan_exit`: present the written plan and leave plan mode (Claude
    /// `ExitPlanMode` parity — the plan is read from disk, not from memory).
    fn execute_plan_exit(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        if !self.plan_mode.load(Ordering::SeqCst) {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail("plan mode is not active")),
            });
        }
        let target = self.resolve_in_root(PLAN_PATH)?;
        let bytes = match fs::read(&target) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&format!(
                        "write the plan to {PLAN_PATH} with workspace_write before plan_exit"
                    ))),
                })
            }
            Err(_) => return Err(ToolStepError::Failed),
        };
        if bytes.is_empty() {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail("the plan file is empty")),
            });
        }
        self.plan_mode.store(false, Ordering::SeqCst);
        let text = String::from_utf8_lossy(&bytes);
        let excerpt: String = text.chars().take(400).collect();
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary: format!(
                "Plan accepted ({} bytes). Plan mode off.\n--- plan ---\n{excerpt}",
                bytes.len()
            ),
        })
    }

    /// `job_status`: background job state without reading its output.
    fn execute_job_status(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_job_id_args(call.arguments(), false)?;
        match self.jobs.snapshot(&args.job_id) {
            Some(state) => Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: format!("{}: {state}", args.job_id),
            }),
            None => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "{}: unknown job id",
                    args.job_id
                ))),
            }),
        }
    }

    /// `job_output`: bounded window into a background job's spooled output.
    fn execute_job_output(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_job_id_args(call.arguments(), true)?;
        let offset = args.offset.unwrap_or(0);
        let Some((text, done, next, state)) = self.jobs.output(&args.job_id, offset) else {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "{}: unknown job id",
                    args.job_id
                ))),
            });
        };
        let mut summary = text;
        if done {
            summary.push_str(&format!("\n[job finished: {state}]"));
        } else {
            summary.push_str(&format!("\n[job {state}; continue at offset {next}]"));
        }
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary,
        })
    }

    /// `task_spawn`: run a subagent (depth 1) and return its final report.
    fn execute_task_spawn(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_task_args(call.arguments())?;
        let Some(runner) = self.subagents.as_ref() else {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(
                    "subagents are not available in this run",
                )),
            });
        };
        match runner.run(&args.prompt, &args.agent_type) {
            Ok(report) => {
                let report = bounded_text(report.as_bytes(), MAX_SUBAGENT_REPORT_BYTES);
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary: format!("subagent ({}) report:\n{}", args.agent_type, report),
                })
            }
            Err(reason) => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "subagent ({}) failed: {reason}",
                    args.agent_type
                ))),
            }),
        }
    }

    /// Group key for write-class calls: same key ⇒ serialized in proposal
    /// order. All `shell_exec` calls share one key (a process may touch any
    /// path); file writes serialize per resolved relative path.
    fn write_group_key(call: &ValidatedToolCall) -> Option<String> {
        match call.tool() {
            SHELL_EXEC_TOOL => Some(SHELL_EXEC_TOOL.to_owned()),
            WORKSPACE_WRITE_TOOL => {
                parse_write_args(call.arguments()).ok().map(|args| args.path)
            }
            WORKSPACE_PATCH_TOOL => parse_patch_args(call.arguments()).ok().map(|a| a.path),
            TODO_WRITE_TOOL => Some(TODOS_PATH.to_owned()),
            _ => None,
        }
    }
}

/// Read/write classification the parallel dispatcher uses. Read-only tools
/// run concurrently with everything; writes serialize per target.
pub fn tool_kind(tool: &str) -> ToolKind {
    match tool {
        WORKSPACE_READ_TOOL | REPO_READ_TOOL | REPO_SEARCH_TOOL | REPO_GLOB_TOOL
        | JOB_STATUS_TOOL | JOB_OUTPUT_TOOL => ToolKind::Read,
        _ => ToolKind::Write,
    }
}

fn tool_class(tool: &str) -> ToolClass {
    match tool {
        WORKSPACE_READ_TOOL | REPO_READ_TOOL | REPO_SEARCH_TOOL | REPO_GLOB_TOOL
        | JOB_STATUS_TOOL | JOB_OUTPUT_TOOL | PLAN_ENTER_TOOL | PLAN_EXIT_TOOL => {
            ToolClass::ReadOnly
        }
        WORKSPACE_WRITE_TOOL | WORKSPACE_PATCH_TOOL | TODO_WRITE_TOOL => ToolClass::FileEdit,
        _ => ToolClass::Other,
    }
}

/// Walk workspace text files depth-first (skipping vendored/build dirs),
/// invoking `visit` with each file's contents; bounded by [`MAX_SEARCH_FILES`].
fn walk_text_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    walked: &mut usize,
    visit: &mut impl FnMut(&str, &str),
) {
    if depth > 16 || *walked >= MAX_SEARCH_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if *walked >= MAX_SEARCH_FILES {
            return;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if file_type.is_dir() {
            if !SEARCH_SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                walk_text_files(root, &entry.path(), depth + 1, walked, visit);
            }
            continue;
        }
        if file_type.is_symlink() || name.starts_with('.') {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        *walked += 1;
        // Binary guard: a NUL byte in the head means "not text" (git heuristic).
        let head = &bytes[..bytes.len().min(8 * 1024)];
        if head.contains(&0u8) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or(name.clone());
        visit(&relative, &String::from_utf8_lossy(&bytes));
    }
}

/// Walk ALL regular files (no text filter) depth-first for `repo_glob`,
/// skipping vendored/build/hidden directories; bounded by [`MAX_SEARCH_FILES`].
fn walk_all_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    walked: &mut usize,
    visit: &mut impl FnMut(&str),
) {
    if depth > 16 || *walked >= MAX_SEARCH_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if *walked >= MAX_SEARCH_FILES {
            return;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if file_type.is_dir() {
            if !SEARCH_SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                walk_all_files(root, &entry.path(), depth + 1, walked, visit);
            }
            continue;
        }
        if file_type.is_symlink() || name.starts_with('.') {
            continue;
        }
        *walked += 1;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or(name.clone());
        visit(&relative);
    }
}

/// Pure relative-path checks shared by validation and execution: non-empty,
/// bounded, no absolute form, no parent/root/prefix components.
fn checked_relative(relative: &str) -> Result<&Path, ToolStepError> {
    if relative.is_empty() || relative.len() > MAX_TOOL_PATH_BYTES {
        return Err(ToolStepError::Invalid);
    }
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolStepError::Invalid);
    }
    Ok(path)
}

/// Largest valid UTF-8 prefix within `cap` bytes, with an explicit
/// truncation marker when content was cut.
fn bounded_text(bytes: &[u8], cap: usize) -> String {
    let mut end = bytes.len().min(cap);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    let mut summary = String::from_utf8(bytes[..end].to_vec()).unwrap_or_default();
    if bytes.len() > end {
        summary.push_str(TRUNCATION_MARKER);
    }
    summary
}

/// Char-boundary-safe cut of model-visible detail text.
fn bounded_detail(text: &str) -> String {
    if text.len() <= MAX_RESULT_DETAIL_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_RESULT_DETAIL_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

struct WriteArgs {
    path: String,
    content: String,
}

struct PatchArgs {
    path: String,
    old: String,
    new: String,
    replace_all: bool,
}

struct ShellArgs {
    argv: Vec<String>,
    timeout: Duration,
    background: bool,
}

struct RepoGlobArgs {
    pattern: String,
    head_limit: usize,
}

struct JobIdArgs {
    job_id: String,
    offset: Option<usize>,
}

struct TaskSpawnArgs {
    prompt: String,
    agent_type: String,
}

struct EmptyArgs;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TodoEntry {
    id: Option<String>,
    content: String,
    status: String,
}

struct TodoArgs {
    todos: Vec<TodoEntry>,
}

struct RepoReadArgs {
    path: String,
    offset: usize,
    limit: usize,
}

struct RepoSearchArgs {
    pattern: String,
    head_limit: usize,
    offset: usize,
}

/// Parse bounded `{"path": ..., "content": ...}` arguments; unknown keys,
/// wrong types, and out-of-bounds values are refused.
fn parse_write_args(raw: &str) -> Result<WriteArgs, ToolStepError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if object.len() != 2 {
        return Err(ToolStepError::Invalid);
    }
    let path = object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    let content = object
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    checked_relative(path)?;
    if content.len() > MAX_WRITE_BYTES {
        return Err(ToolStepError::Invalid);
    }
    Ok(WriteArgs {
        path: path.to_owned(),
        content: content.to_owned(),
    })
}

/// Parse bounded `{"path": ...}` arguments.
fn parse_path_argument(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let path = object.get("path")?.as_str()?;
    checked_relative(path).ok()?;
    Some(path.to_owned())
}

/// Parse bounded `{"path", "old", "new", "replace_all"?}` patch arguments.
/// `old` and `new` must be present, bounded, and different.
fn parse_patch_args(raw: &str) -> Result<PatchArgs, ToolStepError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    let expected = if object.contains_key("replace_all") { 4 } else { 3 };
    if object.len() != expected {
        return Err(ToolStepError::Invalid);
    }
    let path = object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    let old = object
        .get("old")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    let new = object
        .get("new")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    let replace_all = match object.get("replace_all") {
        Some(value) => value.as_bool().ok_or(ToolStepError::Invalid)?,
        None => false,
    };
    checked_relative(path)?;
    if old.is_empty() || old.len() > MAX_PATCH_TEXT_BYTES || new.len() > MAX_PATCH_TEXT_BYTES {
        return Err(ToolStepError::Invalid);
    }
    if old == new {
        return Err(ToolStepError::Invalid);
    }
    Ok(PatchArgs {
        path: path.to_owned(),
        old: old.to_owned(),
        new: new.to_owned(),
        replace_all,
    })
}

/// Parse bounded `{"pattern", "head_limit"?}` glob arguments (Claude `Glob`:
/// head_limit capped at 100; `**` crosses directories, `*` stays in one).
fn parse_repo_glob_args(raw: &str) -> Result<RepoGlobArgs, ToolStepError> {
    const ALLOWED: &[&str] = &["pattern", "head_limit"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if !object.keys().all(|key| ALLOWED.contains(&key.as_str()))
        || !object.contains_key("pattern")
    {
        return Err(ToolStepError::Invalid);
    }
    let pattern = object
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    if pattern.is_empty() || pattern.len() > 256 || pattern.starts_with('/') {
        return Err(ToolStepError::Invalid);
    }
    let head_limit = match object.get("head_limit") {
        Some(value) => {
            let head_limit = value.as_u64().ok_or(ToolStepError::Invalid)?;
            if head_limit == 0 || head_limit as usize > MAX_GLOB_RESULTS {
                return Err(ToolStepError::Invalid);
            }
            head_limit as usize
        }
        None => MAX_GLOB_RESULTS,
    };
    Ok(RepoGlobArgs {
        pattern: pattern.to_owned(),
        head_limit,
    })
}

/// Parse bounded `{"todos": [...]}` task-list arguments. Each entry carries
/// `content` (bounded) and `status`; `id` is optional (merge-by-id when
/// present). Unknown keys, unknown statuses, and bound violations are refused.
fn parse_todo_args(raw: &str) -> Result<TodoArgs, ToolStepError> {
    const STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if !object.contains_key("todos") || object.len() != 1 {
        return Err(ToolStepError::Invalid);
    }
    let entries = object
        .get("todos")
        .and_then(serde_json::Value::as_array)
        .ok_or(ToolStepError::Invalid)?;
    if entries.is_empty() || entries.len() > MAX_TODOS {
        return Err(ToolStepError::Invalid);
    }
    let mut todos = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry.as_object().ok_or(ToolStepError::Invalid)?;
        if entry.len() > 3 {
            return Err(ToolStepError::Invalid);
        }
        let content = entry
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or(ToolStepError::Invalid)?;
        if content.is_empty() || content.len() > MAX_TODO_CONTENT_BYTES {
            return Err(ToolStepError::Invalid);
        }
        let status = entry
            .get("status")
            .and_then(serde_json::Value::as_str)
            .ok_or(ToolStepError::Invalid)?;
        if !STATUSES.contains(&status) {
            return Err(ToolStepError::Invalid);
        }
        let id = match entry.get("id") {
            Some(id) => Some(id.as_str().ok_or(ToolStepError::Invalid)?.to_owned()),
            None => None,
        };
        todos.push(TodoEntry {
            id,
            content: content.to_owned(),
            status: status.to_owned(),
        });
    }
    Ok(TodoArgs { todos })
}

/// Parse bounded `{"job_id", "offset"?}` background-job arguments.
fn parse_job_id_args(raw: &str, with_offset: bool) -> Result<JobIdArgs, ToolStepError> {
    const ALLOWED: &[&str] = &["job_id", "offset"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    let allowed: &[&str] = if with_offset {
        ALLOWED
    } else {
        &["job_id"]
    };
    if !object.keys().all(|key| allowed.contains(&key.as_str()))
        || !object.contains_key("job_id")
    {
        return Err(ToolStepError::Invalid);
    }
    let job_id = object
        .get("job_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    if job_id.is_empty() || job_id.len() > 64 {
        return Err(ToolStepError::Invalid);
    }
    let offset = match object.get("offset") {
        Some(value) => {
            let offset = value.as_u64().ok_or(ToolStepError::Invalid)?;
            offset as usize
        }
        None => 0,
    };
    Ok(JobIdArgs {
        job_id: job_id.to_owned(),
        offset: Some(offset),
    })
}

/// Parse `{}` — plan mode switches take no arguments.
fn parse_empty_args(raw: &str) -> Result<EmptyArgs, ToolStepError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    if !value.as_object().is_some_and(|object| object.is_empty()) {
        return Err(ToolStepError::Invalid);
    }
    Ok(EmptyArgs)
}

/// Parse bounded `{"prompt", "type"?, "description"?}` subagent arguments.
/// Types adopt the reference-CLI standard: general-purpose | explore | plan.
fn parse_task_args(raw: &str) -> Result<TaskSpawnArgs, ToolStepError> {
    const ALLOWED: &[&str] = &["prompt", "type", "description"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if !object.keys().all(|key| ALLOWED.contains(&key.as_str()))
        || !object.contains_key("prompt")
    {
        return Err(ToolStepError::Invalid);
    }
    let prompt = object
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    if prompt.is_empty() || prompt.len() > MAX_SPAWN_PROMPT_BYTES {
        return Err(ToolStepError::Invalid);
    }
    let agent_type = match object.get("type") {
        Some(value) => {
            let raw_type = value.as_str().ok_or(ToolStepError::Invalid)?;
            if !AGENT_TYPES.contains(&raw_type) {
                return Err(ToolStepError::Invalid);
            }
            raw_type.to_owned()
        }
        None => "general-purpose".to_owned(),
    };
    Ok(TaskSpawnArgs {
        prompt: prompt.to_owned(),
        agent_type,
    })
}

/// Segment-aware path glob: `**` matches zero or more whole directories,
/// `*`/`?` stay inside one segment. `*.rs` matches top-level Rust files only;
/// `**/*.rs` matches Rust files at any depth.
pub fn glob_path_match(pattern: &str, path: &str) -> bool {
    fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((segment, rest)) if *segment == "**" => {
                for skip in 0..=path.len() {
                    if match_segments(rest, &path[skip..]) {
                        return true;
                    }
                }
                false
            }
            Some((segment, rest)) => {
                let Some(first) = path.split_first() else {
                    return false;
                };
                segment_glob(segment, first.0) && match_segments(rest, first.1)
            }
        }
    }
    fn segment_glob(pattern: &str, value: &str) -> bool {
        let pattern: Vec<char> = pattern.chars().collect();
        let value: Vec<char> = value.chars().collect();
        let (mut p, mut v) = (0usize, 0usize);
        let mut star: Option<usize> = None;
        let mut star_v = 0usize;
        while v < value.len() {
            if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
                p += 1;
                v += 1;
            } else if p < pattern.len() && pattern[p] == '*' {
                star = Some(p);
                star_v = v;
                p += 1;
            } else if let Some(star_p) = star {
                p = star_p + 1;
                star_v += 1;
                v = star_v;
            } else {
                return false;
            }
        }
        while p < pattern.len() && pattern[p] == '*' {
            p += 1;
        }
        p == pattern.len()
    }
    let pattern: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    match_segments(&pattern, &path)
}

/// Parse bounded `{"argv": [...], "timeout_ms"?}` arguments. Argv-only: a
/// shell string is never interpreted, so every token is bounded and free of
/// control characters.
fn parse_shell_args(raw: &str) -> Result<ShellArgs, ToolStepError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    let expected = 1 + usize::from(object.contains_key("timeout_ms"))
        + usize::from(object.contains_key("background"));
    if object.len() != expected {
        return Err(ToolStepError::Invalid);
    }
    let argv = object
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .ok_or(ToolStepError::Invalid)?;
    if argv.is_empty() || argv.len() > MAX_SHELL_ARGV {
        return Err(ToolStepError::Invalid);
    }
    let mut tokens = Vec::with_capacity(argv.len());
    for token in argv {
        let token = token.as_str().ok_or(ToolStepError::Invalid)?;
        if token.is_empty()
            || token.len() > MAX_SHELL_ARG_BYTES
            || token.bytes().any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(ToolStepError::Invalid);
        }
        tokens.push(token.to_owned());
    }
    let timeout = match object.get("timeout_ms") {
        Some(value) => {
            let millis = value.as_u64().ok_or(ToolStepError::Invalid)?;
            if millis == 0 || Duration::from_millis(millis) > MAX_SHELL_TIMEOUT {
                return Err(ToolStepError::Invalid);
            }
            Duration::from_millis(millis)
        }
        None => DEFAULT_SHELL_TIMEOUT,
    };
    let background = match object.get("background") {
        Some(value) => value.as_bool().ok_or(ToolStepError::Invalid)?,
        None => false,
    };
    Ok(ShellArgs {
        argv: tokens,
        timeout,
        background,
    })
}

/// Parse bounded `{"path", "offset"?, "limit"?}` paginated-read arguments.
/// Unknown keys are refused.
fn parse_repo_read_args(raw: &str) -> Result<RepoReadArgs, ToolStepError> {
    const ALLOWED: &[&str] = &["path", "offset", "limit"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if !object.keys().all(|key| ALLOWED.contains(&key.as_str()))
        || !object.contains_key("path")
    {
        return Err(ToolStepError::Invalid);
    }
    let path = object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    checked_relative(path)?;
    let offset = match object.get("offset") {
        Some(value) => {
            let offset = value.as_u64().ok_or(ToolStepError::Invalid)?;
            if offset == 0 || offset > u32::MAX as u64 {
                return Err(ToolStepError::Invalid);
            }
            offset as usize
        }
        None => DEFAULT_READ_OFFSET,
    };
    let limit = match object.get("limit") {
        Some(value) => {
            let limit = value.as_u64().ok_or(ToolStepError::Invalid)?;
            if limit == 0 || limit as usize > MAX_READ_LINES {
                return Err(ToolStepError::Invalid);
            }
            limit as usize
        }
        None => DEFAULT_READ_LINES,
    };
    Ok(RepoReadArgs {
        path: path.to_owned(),
        offset,
        limit,
    })
}

/// Parse bounded `{"pattern", "head_limit"?, "offset"?}` search arguments.
/// Unknown keys are refused.
fn parse_repo_search_args(raw: &str) -> Result<RepoSearchArgs, ToolStepError> {
    const ALLOWED: &[&str] = &["pattern", "head_limit", "offset"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if !object.keys().all(|key| ALLOWED.contains(&key.as_str()))
        || !object.contains_key("pattern")
    {
        return Err(ToolStepError::Invalid);
    }
    let pattern = object
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    if pattern.is_empty() || pattern.len() > 256 {
        return Err(ToolStepError::Invalid);
    }
    let head_limit = match object.get("head_limit") {
        Some(value) => {
            let head_limit = value.as_u64().ok_or(ToolStepError::Invalid)?;
            if head_limit == 0 || head_limit as usize > MAX_SEARCH_HEAD_LIMIT {
                return Err(ToolStepError::Invalid);
            }
            head_limit as usize
        }
        None => DEFAULT_SEARCH_HEAD_LIMIT,
    };
    let offset = match object.get("offset") {
        Some(value) => {
            let offset = value.as_u64().ok_or(ToolStepError::Invalid)?;
            if offset > u32::MAX as u64 {
                return Err(ToolStepError::Invalid);
            }
            offset as usize
        }
        None => 0,
    };
    Ok(RepoSearchArgs {
        pattern: pattern.to_owned(),
        head_limit,
        offset,
    })
}

/// Tool surface for one exec run: the workspace driver when the project is
/// trusted, otherwise the fail-closed no-op surface that refuses every call.
pub enum ExecTools {
    Noop(NoopTools),
    Workspace(WorkspaceTools),
}

impl ExecTools {
    /// The untrusted surface: every proposed tool call is refused.
    pub fn noop() -> Self {
        Self::Noop(NoopTools)
    }

    /// The trusted workspace surface rooted at `root`, with the default
    /// permission lattice.
    pub fn workspace(root: &Path) -> Result<Self, ToolSetupError> {
        Ok(Self::Workspace(WorkspaceTools::open(root)?))
    }

    /// Read-only trusted surface (subagent explore/plan scopes).
    pub fn read_only(root: &Path) -> Result<Self, ToolSetupError> {
        Ok(Self::Workspace(WorkspaceTools::open_read_only(root)?))
    }

    /// Attach the subagent runner (no-op on the fail-closed no-op surface).
    pub fn set_subagent_runner(&mut self, runner: std::sync::Arc<dyn SubagentRunner>) {
        if let Self::Workspace(tools) = self {
            tools.set_subagent_runner(runner);
        }
    }

    /// The trusted workspace surface with an explicit permission lattice.
    pub fn workspace_with_permissions(
        root: &Path,
        permissions: PermissionLattice,
    ) -> Result<Self, ToolSetupError> {
        Ok(Self::Workspace(WorkspaceTools::open_with_permissions(
            root,
            permissions,
        )?))
    }
}

/// Model-visible JSON schema for one tool's arguments.
fn arguments_schema(
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type": "object",
        "description": description,
        "properties": properties,
        "additionalProperties": false,
    });
    schema["required"] = serde_json::json!(required);
    schema
}

impl ToolDriver for WorkspaceTools {
    fn tool_surface(&self) -> Vec<ToolSurface> {
        // Read-only scopes (subagent explore/plan) advertise only
        // read-classified tools.
        let mut surface = self.full_surface_impl();
        if self.read_only {
            surface.retain(|tool| tool_kind(tool.name()) == ToolKind::Read);
        }
        surface
    }

    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        cancel.check().map_err(|_| ToolStepError::Cancelled)?;
        // Known tools accept the call here even with malformed arguments:
        // execute renders the failure as a per-call model-visible result the
        // model can correct. Unknown tools are structural refusals.
        if !matches!(
            call.tool(),
            WORKSPACE_WRITE_TOOL
                | WORKSPACE_READ_TOOL
                | REPO_READ_TOOL
                | REPO_SEARCH_TOOL
                | WORKSPACE_PATCH_TOOL
                | SHELL_EXEC_TOOL
                | REPO_GLOB_TOOL
                | TODO_WRITE_TOOL
                | PLAN_ENTER_TOOL
                | PLAN_EXIT_TOOL
                | JOB_STATUS_TOOL
                | JOB_OUTPUT_TOOL
                | TASK_SPAWN_TOOL
        ) {
            return Err(ToolStepError::Invalid);
        }
        Ok(ValidatedToolCall::from_proposed(call))
    }


    fn execute(
        &mut self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        self.execute_call(call, cancel)
    }

    fn tool_kind(&self, tool: &str) -> ToolKind {
        tool_kind(tool)
    }

    fn execute_batch(
        &mut self,
        calls: &[ValidatedToolCall],
        cancel: &CancellationToken,
    ) -> Vec<Result<ToolStepResult, ToolStepError>> {
        batch_dispatch(self, calls, cancel)
    }
}

/// Threaded batch dispatch over the workspace driver: read-classified calls
/// run individually and concurrently, write-classified calls group by target
/// key (all `shell_exec` together, file writes per relative path) so
/// same-path writes serialize in proposal order. Results keep per-call order,
/// ids, and outcomes.
fn batch_dispatch(
    tools: &WorkspaceTools,
    calls: &[ValidatedToolCall],
    cancel: &CancellationToken,
) -> Vec<Result<ToolStepResult, ToolStepError>> {
    let mut groups: Vec<(Option<String>, Vec<usize>)> = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        let key = if tool_kind(call.tool()) == ToolKind::Read {
            None
        } else {
            WorkspaceTools::write_group_key(call)
        };
        if let Some(group) = groups.iter_mut().find(|(existing, _)| *existing == key) {
            group.1.push(index);
        } else {
            groups.push((key, vec![index]));
        }
    }
    let mut done: Vec<Vec<(usize, Result<ToolStepResult, ToolStepError>)>> = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (_, indexes) in groups {
            handles.push(scope.spawn(move || {
                indexes
                    .into_iter()
                    .map(|index| {
                        let outcome = tools.execute_call(&calls[index], cancel);
                        (index, outcome)
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for handle in handles {
            if let Ok(group) = handle.join() {
                done.push(group);
            }
        }
    });
    let mut outcomes: Vec<Option<Result<ToolStepResult, ToolStepError>>> =
        (0..calls.len()).map(|_| None).collect();
    for group in done {
        for (index, outcome) in group {
            outcomes[index] = Some(outcome);
        }
    }
    outcomes
        .into_iter()
        .map(|outcome| outcome.unwrap_or(Err(ToolStepError::Failed)))
        .collect()
}

impl ToolDriver for ExecTools {
    fn tool_surface(&self) -> Vec<ToolSurface> {
        match self {
            Self::Noop(tools) => tools.tool_surface(),
            Self::Workspace(tools) => tools.tool_surface(),
        }
    }

    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        match self {
            Self::Noop(tools) => tools.validate(call, cancel),
            Self::Workspace(tools) => tools.validate(call, cancel),
        }
    }

    fn execute(
        &mut self,
        call: &ValidatedToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        match self {
            Self::Noop(tools) => tools.execute(call, cancel),
            Self::Workspace(tools) => tools.execute(call, cancel),
        }
    }

    fn tool_kind(&self, tool: &str) -> ToolKind {
        tool_kind(tool)
    }

    fn execute_batch(
        &mut self,
        calls: &[ValidatedToolCall],
        cancel: &CancellationToken,
    ) -> Vec<Result<ToolStepResult, ToolStepError>> {
        let Self::Workspace(tools) = self else {
            // The no-op surface has no side effects to parallelize; the
            // default sequential dispatch refuses each call.
            return calls
                .iter()
                .map(|call| self.execute(call, cancel))
                .collect();
        };
        batch_dispatch(tools, calls, cancel)
    }
}
impl WorkspaceTools {
    fn full_surface_impl(&self) -> Vec<ToolSurface> {
        vec![
            ToolSurface::new(
                WORKSPACE_WRITE_TOOL,
                "Create or overwrite a UTF-8 text file inside the trusted workspace. \
                 Arguments JSON: {\"path\":\"<workspace-relative path>\",\"content\":\"<text>\"}.",
                arguments_schema(
                    "Write a workspace file",
                    serde_json::json!({
                        "path": {"type": "string", "description": "workspace-relative file path"},
                        "content": {"type": "string"}
                    }),
                    &["path", "content"],
                ),
            ),
            ToolSurface::new(
                WORKSPACE_READ_TOOL,
                "Read a UTF-8 text file inside the trusted workspace; the returned \
                 content is bounded. Arguments JSON: {\"path\":\"<workspace-relative path>\"}.",
                arguments_schema(
                    "Read a workspace file",
                    serde_json::json!({
                        "path": {"type": "string", "description": "workspace-relative file path"}
                    }),
                    &["path"],
                ),
            ),
            ToolSurface::new(
                REPO_READ_TOOL,
                "Read a bounded window of lines from a workspace file, 1-indexed. \
                 Arguments JSON: {\"path\":\"<file>\",\"offset\":<first line, default 1>,\
                 \"limit\":<lines, default 200, max 1000>}.",
                arguments_schema(
                    "Read a line window of a workspace file",
                    serde_json::json!({
                        "path": {"type": "string", "description": "workspace-relative file path"},
                        "offset": {"type": "integer", "description": "1-indexed first line"},
                        "limit": {"type": "integer", "description": "lines to return"}
                    }),
                    &["path"],
                ),
            ),
            ToolSurface::new(
                REPO_SEARCH_TOOL,
                "Search workspace text files for an exact substring; returns \
                 \"path:line: text\" hits paginated by head_limit/offset. Arguments JSON: \
                 {\"pattern\":\"<exact text>\",\"head_limit\":<default 20, max 100>,\
                 \"offset\":<default 0>}.",
                arguments_schema(
                    "Search workspace files",
                    serde_json::json!({
                        "pattern": {"type": "string", "description": "exact substring to find"},
                        "head_limit": {"type": "integer", "description": "hits to return"},
                        "offset": {"type": "integer", "description": "hits to skip"}
                    }),
                    &["pattern"],
                ),
            ),
            ToolSurface::new(
                WORKSPACE_PATCH_TOOL,
                "Replace an exact substring in a workspace file. old must match exactly \
                 once unless replace_all is true, and must differ from new; old and new are \
                 each capped at 3072 bytes. Arguments JSON: \
                 {\"path\":\"<file>\",\"old\":\"<exact text>\",\"new\":\"<replacement>\",\
                 \"replace_all\":<optional bool>}.",
                arguments_schema(
                    "Edit a workspace file by exact match",
                    serde_json::json!({
                        "path": {"type": "string", "description": "workspace-relative file path"},
                        "old": {"type": "string", "description": "exact text to replace"},
                        "new": {"type": "string", "description": "replacement text"},
                        "replace_all": {"type": "boolean", "description": "replace every occurrence"}
                    }),
                    &["path", "old", "new"],
                ),
            ),
            ToolSurface::new(
                REPO_GLOB_TOOL,
                "Find workspace files by glob pattern: `**/*.rs` matches at any depth,                  `*.rs` only at the workspace root; results capped at 100. Arguments JSON:                  {\"pattern\":\"**/*.rs\",\"head_limit\":<optional, max 100>}.",
                arguments_schema(
                    "Find files by glob pattern",
                    serde_json::json!({
                        "pattern": {"type": "string", "description": "glob such as **/*.rs"},
                        "head_limit": {"type": "integer", "description": "results to return"}
                    }),
                    &["pattern"],
                ),
            ),
            ToolSurface::new(
                TODO_WRITE_TOOL,
                "Maintain your task list for this workspace: pass the full set of tasks with                  status pending | in_progress | completed | cancelled; entries with an id                  update that task, entries without one are added. Arguments JSON:                  {\"todos\":[{\"id\":\"1\",\"content\":\"...\",\"status\":\"in_progress\"}]}.",
                arguments_schema(
                    "Update the task list",
                    serde_json::json!({
                        "todos": {"type": "array", "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "content": {"type": "string"},
                                "status": {"type": "string",
                                           "enum": ["pending", "in_progress",
                                                    "completed", "cancelled"]}
                            },
                            "required": ["content", "status"]
                        }, "description": "full task list (merge-by-id)"}
                    }),
                    &["todos"],
                ),
            ),
            ToolSurface::new(
                PLAN_ENTER_TOOL,
                "Enter plan mode: research freely, but every write except the plan file                  (.rapidlm/plan.md) is refused. Write the plan with workspace_write to that                  path, then call plan_exit. Arguments JSON: {}.",
                arguments_schema("Enter plan mode", serde_json::json!({}), &[]),
            ),
            ToolSurface::new(
                PLAN_EXIT_TOOL,
                "Leave plan mode: the plan is read from .rapidlm/plan.md on disk (must exist                  and be non-empty) and returned for approval. Arguments JSON: {}.",
                arguments_schema("Exit plan mode with the written plan", serde_json::json!({}), &[]),
            ),
            ToolSurface::new(
                JOB_STATUS_TOOL,
                "Check a background job started with shell_exec background=true: returns                  running/completed/failed. Arguments JSON: {\"job_id\":\"job-1\"}.",
                arguments_schema(
                    "Check background job status",
                    serde_json::json!({
                        "job_id": {"type": "string", "description": "job id such as job-1"}
                    }),
                    &["job_id"],
                ),
            ),
            ToolSurface::new(
                JOB_OUTPUT_TOOL,
                "Read the spooled output of a background job from `offset`. Arguments JSON:                  {\"job_id\":\"job-1\",\"offset\":0}.",
                arguments_schema(
                    "Read background job output",
                    serde_json::json!({
                        "job_id": {"type": "string", "description": "job id such as job-1"},
                        "offset": {"type": "integer", "description": "byte offset to read from"}
                    }),
                    &["job_id"],
                ),
            ),
            ToolSurface::new(
                TASK_SPAWN_TOOL,
                "Spawn a subagent (depth 1: it cannot spawn further agents) for a focused                  task and return its final report. Types: general-purpose (full tools),                  explore (read-only), plan (read-only). Arguments JSON:                  {\"prompt\":\"<task>\",\"type\":\"explore\"}.",
                arguments_schema(
                    "Spawn a subagent for a focused task",
                    serde_json::json!({
                        "prompt": {"type": "string", "description": "the subagent's task"},
                        "type": {"type": "string",
                                 "enum": ["general-purpose", "explore", "plan"],
                                 "description": "subagent type"},
                        "description": {"type": "string", "description": "short label"}
                    }),
                    &["prompt"],
                ),
            ),
            ToolSurface::new(
                SHELL_EXEC_TOOL,
                "Run one command inside the workspace root (argv form, no shell). Bounded \
                 output capture and a wall-clock timeout. Arguments JSON: \
                 {\"argv\":[\"<program>\",\"<arg>\",...],\"timeout_ms\":<optional, \
                 default 60000, max 600000>}.",
                arguments_schema(
                    "Run a supervised command",
                    serde_json::json!({
                        "argv": {"type": "array", "items": {"type": "string"},
                                 "description": "program and arguments, argv form"},
                        "timeout_ms": {"type": "integer", "description": "wall-clock timeout"}
                    }),
                    &["argv"],
                ),
            ),
        ]
    }
}

/// A [`ToolDriver`] with no tool gateway configured. Structural tool calls are
/// never executed; used when the project is not trusted (fail-closed).
pub struct NoopTools;

impl ToolDriver for NoopTools {
    fn validate(
        &mut self,
        _call: &ProposedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ValidatedToolCall, ToolStepError> {
        Err(ToolStepError::Invalid)
    }
    fn execute(
        &mut self,
        _call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        Err(ToolStepError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{PreservedLiveContext, run_live_exec};
    use crate::permissions::{
        RuleEffect, ToolPattern, ToolRule,
    };
    use agent_runtime::{
        AgentExecutionRequest, AgentRole, AgentSpec, AgentTerminalStatus, ContextRetryPolicy,
        ModelStepError, ModelStepInput, ModelStepOutput,
    };
    use protocol::{AgentId, SessionId, WorkspaceViewId};
    use std::collections::VecDeque;
    use std::sync::mpsc;
use std::sync::{Arc, Mutex};

    /// Temp workspace root removed on drop.
    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rapid-exec-tools-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.subsec_nanos())
                    .unwrap_or(0)
            ));
            fs::create_dir_all(&dir).expect("temp root");
            Self(dir)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Scripted live model: proposed tool calls, then a terminal answer.
    struct ScriptedModel {
        outputs: VecDeque<Result<ModelStepOutput, ModelStepError>>,
    }
    impl ScriptedModel {
        fn write_then_answer(path: &str, content: &str, answer: &str) -> Self {
            let call = ProposedToolCall::new(
                "c1",
                WORKSPACE_WRITE_TOOL,
                format!(r#"{{"path":"{path}","content":"{content}"}}"#),
            )
            .expect("call");
            Self {
                outputs: VecDeque::from(vec![
                    Ok(ModelStepOutput::ToolCalls {
                        calls: vec![call],
                        tokens: 1,
                    }),
                    Ok(ModelStepOutput::Terminal {
                        text: answer.to_owned(),
                        tokens: 1,
                    }),
                ]),
            }
        }

        fn calls_then_answer(calls: Vec<ProposedToolCall>, answer: &str) -> Self {
            Self {
                outputs: VecDeque::from(vec![
                    Ok(ModelStepOutput::ToolCalls { calls, tokens: 1 }),
                    Ok(ModelStepOutput::Terminal {
                        text: answer.to_owned(),
                        tokens: 1,
                    }),
                ]),
            }
        }
    }
    impl crate::host::LiveModelCall for ScriptedModel {
        fn step(
            &mut self,
            _blocks: &[context_engine::compile::ContextBlock],
            _input: &ModelStepInput<'_>,
            _cancel: &CancellationToken,
        ) -> Result<ModelStepOutput, ModelStepError> {
            self.outputs
                .pop_front()
                .unwrap_or(Err(ModelStepError::Failed))
        }
    }

    fn exec_request() -> AgentExecutionRequest {
        let spec = AgentSpec::builder(
            AgentId::new(),
            AgentRole::Coder,
            "create a file",
            WorkspaceViewId::new(),
        )
        .permissions_profile("work")
        .build()
        .expect("spec");
        AgentExecutionRequest::new(spec, SessionId::new())
    }

    fn preserved() -> PreservedLiveContext {
        PreservedLiveContext::new("create a file", Vec::new(), "", "", 8192, 256).expect("preserved")
    }

    fn make_call(id: &str, tool: &str, arguments: &str) -> ProposedToolCall {
        ProposedToolCall::new(id, tool, arguments).expect("call")
    }

    /// Workspace driver with a bypassPermissions lattice: for tests of tool
    /// mechanics (the permission gate has its own dedicated tests).
    fn permissive_workspace(root: &Path) -> WorkspaceTools {
        WorkspaceTools::open_with_permissions(
            root,
            PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions),
        )
        .expect("tools")
    }

    fn validate_one(tools: &mut ExecTools, call: &ProposedToolCall) -> ValidatedToolCall {
        tools.validate(call, &CancellationToken::new()).expect("validate")
    }

    fn run_batch(
        tools: &mut ExecTools,
        calls: &[ProposedToolCall],
    ) -> Vec<Result<ToolStepResult, ToolStepError>> {
        let validated: Vec<ValidatedToolCall> = calls
            .iter()
            .map(|call| validate_one(tools, call))
            .collect();
        tools.execute_batch(&validated, &CancellationToken::new())
    }

    #[test]
    fn write_then_read_roundtrip_stays_inside_the_root() {
        let root = TempRoot::new("roundtrip");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"src/main.rs","content":"fn main() { println!(\"hi\"); }"}"#,
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("execute");
        assert!(matches!(result, ToolStepResult::Succeeded { .. }));
        let written = fs::read(root.0.join("src/main.rs")).expect("file exists");
        assert_eq!(written, b"fn main() { println!(\"hi\"); }");

        let read = ProposedToolCall::new(
            "c2",
            WORKSPACE_READ_TOOL,
            r#"{"path":"src/main.rs"}"#,
        )
        .expect("call");
        let validated = tools.validate(&read, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("execute");
        match result {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("fn main()"));
            }
            other => panic!("expected read success, got {other:?}"),
        }
    }

    #[test]
    fn traversal_absolute_and_oversize_arguments_are_refused() {
        // Malformed or oversized arguments are per-call handled failures the
        // model can correct; the turn survives and nothing is written.
        let root = TempRoot::new("refuse");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        for arguments in [
            r#"{"path":"../escape.txt","content":"x"}"#,
            r#"{"path":"/etc/passwd","content":"x"}"#,
            r#"{"path":"a/../../out.txt","content":"x"}"#,
            r#"{"path":"","content":"x"}"#,
            r#"{"path":"ok.txt","content":"x","extra":1}"#,
            r#"{"path":42,"content":"x"}"#,
            // Sized past the tool layer's argument bound but under the turn
            // layer's own 16 KiB cap, so this refusal is ours.
            format!(
                r#"{{"path":"big.txt","content":"{}"}}"#,
                "x".repeat(MAX_TOOL_ARGUMENTS_BYTES + 1)
            )
            .as_str(),
        ] {
            let call =
                ProposedToolCall::new("c1", WORKSPACE_WRITE_TOOL, arguments).expect("call");
            let validated = tools.validate(&call, &cancel).expect("known tool validates");
            let outcome = tools.execute(&validated, &cancel).expect("handled");
            match outcome {
                ToolStepResult::Failed { handled, detail, .. } => {
                    assert!(handled, "{arguments}");
                    assert!(!detail.unwrap().is_empty(), "{arguments}");
                }
                other => panic!("expected handled refusal for {arguments}, got {other:?}"),
            }
        }
        // Nothing was written anywhere.
        assert_eq!(fs::read_dir(&root.0).expect("root").count(), 0);
    }

    #[test]
    fn unknown_tools_are_refused() {
        let root = TempRoot::new("unknown");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new("c1", "mcp.call", "{}").expect("call");
        assert!(matches!(
            tools.validate(&call, &cancel),
            Err(ToolStepError::Invalid)
        ));
    }

    #[test]
    fn missing_read_is_a_handled_model_visible_failure() {
        let root = TempRoot::new("missing");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new("c1", WORKSPACE_READ_TOOL, r#"{"path":"nope.txt"}"#)
            .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("handled");
        match result {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("not found"));
            }
            other => panic!("expected handled failure, got {other:?}"),
        }
    }

    #[test]
    fn oversized_reads_are_truncated_with_a_marker() {
        assert_eq!(bounded_text(b"hello", MAX_READ_BYTES), "hello");
        let big = vec![b'a'; MAX_READ_BYTES + 10];
        let text = bounded_text(&big, MAX_READ_BYTES);
        assert_eq!(text.len(), MAX_READ_BYTES + TRUNCATION_MARKER.len());
        assert!(text.ends_with("[truncated]"));
        // Invalid UTF-8 tails are cut on a char boundary, never panicked.
        let mut mixed = vec![b'x'; 4];
        mixed.extend_from_slice(&[0xf0, 0x9f, 0x92]); // cut emoji
        assert!(!bounded_text(&mixed, MAX_READ_BYTES).contains('\u{fffd}'));
    }

    #[test]
    fn repo_read_returns_the_requested_offset_limit_window() {
        let root = TempRoot::new("repo-read");
        fs::write(root.0.join("lines.txt"), (1..=30).map(|n| n.to_string())
            .collect::<Vec<_>>().join("\n"))
            .expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        let call = make_call(
            "c1",
            REPO_READ_TOOL,
            r#"{"path":"lines.txt","offset":10,"limit":5}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                let lines: Vec<&str> = summary.lines().collect();
                assert_eq!(lines[..5], ["10", "11", "12", "13", "14"]);
                assert_eq!(lines[5], "[truncated] (lines 10-14 of 30)");
            }
            other => panic!("expected read success, got {other:?}"),
        }

        // Window past EOF clamps to the remaining lines and reports the range.
        let call = make_call(
            "c2",
            REPO_READ_TOOL,
            r#"{"path":"lines.txt","offset":28,"limit":50}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("28"));
                assert!(summary.contains("lines 28-30 of 30"), "{summary}");
            }
            other => panic!("expected clamped read, got {other:?}"),
        }

        // Zero offset/limit and unknown keys are per-call handled failures
        // (offset is 1-indexed, limit > 0, no unknown keys).
        for arguments in [
            r#"{"path":"lines.txt","offset":0}"#,
            r#"{"path":"lines.txt","limit":0}"#,
            r#"{"path":"lines.txt","limit":1001}"#,
            r#"{"path":"lines.txt","extra":1}"#,
        ] {
            let call = make_call("c3", REPO_READ_TOOL, arguments);
            let validated = tools.validate(&call, &cancel).expect("known tool validates");
            match tools.execute(&validated, &cancel).expect("handled") {
                ToolStepResult::Failed { handled, .. } => assert!(handled, "{arguments}"),
                other => panic!("expected handled refusal for {arguments}, got {other:?}"),
            }
        }
    }

    #[test]
    fn repo_read_byte_cut_reports_the_delivered_window_not_the_requested_one() {
        // A page whose lines exceed the byte cap must report how many lines
        // were actually delivered and where to continue — never claim the
        // full requested window.
        let root = TempRoot::new("repo-read-honest");
        let lines: Vec<String> = (1..=100)
            .map(|n| format!("line-{n:04} {}", "x".repeat(180)))
            .collect();
        fs::write(root.0.join("wide.txt"), lines.join("\n")).expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = make_call(
            "c1",
            REPO_READ_TOOL,
            r#"{"path":"wide.txt","offset":1,"limit":100}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("[truncated]"), "{summary}");
                let last_line = summary.lines().last().expect("marker line");
                // Shape: "[truncated] (byte cap: lines 1-<N> of 100
                // delivered; continue at <N+1>)".
                let delivered: usize = last_line
                    .split("lines 1-")
                    .nth(1)
                    .and_then(|rest| rest.split(" of").next())
                    .and_then(|number| number.parse::<usize>().ok())
                    .expect("delivered window parseable");
                assert!(delivered < 100, "the page must have been byte-cut");
                assert!(
                    last_line.contains(&format!(
                        "lines 1-{delivered} of 100 delivered; continue at {}",
                        delivered + 1
                    )),
                    "marker must name the delivered window: {last_line}"
                );
            }
            other => panic!("expected capped read, got {other:?}"),
        }
    }

    #[test]
    fn repo_read_of_a_large_file_is_byte_capped_with_a_marker() {
        let root = TempRoot::new("repo-read-cap");
        fs::write(root.0.join("big.txt"), "x".repeat(MAX_READ_BYTES * 2)).expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = make_call("c1", REPO_READ_TOOL, r#"{"path":"big.txt"}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains(TRUNCATION_MARKER));
                assert!(summary.len() < MAX_READ_BYTES * 2);
            }
            other => panic!("expected capped read, got {other:?}"),
        }
    }

    #[test]
    fn repo_search_honors_head_limit_and_offset() {
        let root = TempRoot::new("repo-search");
        for index in 0..5 {
            fs::write(
                root.0.join(format!("mod{index}.txt")),
                format!("needle here {index}\nother line\n"),
            )
            .expect("seed");
        }
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        let call = make_call("c1", REPO_SEARCH_TOOL, r#"{"pattern":"needle"}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert_eq!(summary.matches("needle").count(), 5, "{summary}");
                for index in 0..5 {
                    assert!(summary.contains(&format!("mod{index}.txt:1:")));
                }
            }
            other => panic!("expected hits, got {other:?}"),
        }

        // head_limit bounds the page and reports the remainder.
        let call = make_call(
            "c2",
            REPO_SEARCH_TOOL,
            r#"{"pattern":"needle","head_limit":2}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert_eq!(summary.matches("needle").count(), 2);
                assert!(summary.contains("[more hits: 3/5]"), "{summary}");
            }
            other => panic!("expected bounded page, got {other:?}"),
        }

        // offset skips the first hits in a defined (sorted) order.
        let call = make_call(
            "c3",
            REPO_SEARCH_TOOL,
            r#"{"pattern":"needle","head_limit":2,"offset":3}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("mod3.txt"), "{summary}");
                assert!(summary.contains("mod4.txt"), "{summary}");
                assert!(!summary.contains("mod0.txt"), "{summary}");
            }
            other => panic!("expected offset page, got {other:?}"),
        }

        // Bad bounds are per-call handled failures.
        for arguments in [
            r#"{"pattern":""}"#,
            r#"{"head_limit":0,"pattern":"x"}"#,
            r#"{"pattern":"x","head_limit":101}"#,
            r#"{"pattern":"x","offset":-1}"#,
            r#"{"pattern":"x","extra":1}"#,
        ] {
            let call = make_call("c4", REPO_SEARCH_TOOL, arguments);
            let validated = tools.validate(&call, &cancel).expect("known tool validates");
            match tools.execute(&validated, &cancel).expect("handled") {
                ToolStepResult::Failed { handled, .. } => assert!(handled, "{arguments}"),
                other => panic!("expected handled refusal for {arguments}, got {other:?}"),
            }
        }
    }

    #[test]
    fn repo_search_skips_vendored_dirs_binary_and_dotfiles() {
        let root = TempRoot::new("repo-search-skip");
        fs::write(root.0.join("visible.txt"), "needle\n").expect("seed");
        fs::create_dir_all(root.0.join("target")).expect("mkdir");
        fs::write(root.0.join("target/debug.txt"), "needle\n").expect("seed");
        fs::create_dir_all(root.0.join(".git")).expect("mkdir");
        fs::write(root.0.join(".git/config"), "needle\n").expect("seed");
        fs::write(root.0.join(".hidden.txt"), "needle\n").expect("seed");
        fs::write(root.0.join("binary.bin"), [0u8, b'n', b'e']).expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = make_call("c1", REPO_SEARCH_TOOL, r#"{"pattern":"needle"}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("visible.txt:1:"), "{summary}");
                assert!(!summary.contains("target"), "{summary}");
                assert!(!summary.contains(".git"), "{summary}");
                assert!(!summary.contains("hidden"), "{summary}");
                assert!(!summary.contains("binary.bin"), "{summary}");
            }
            other => panic!("expected one hit, got {other:?}"),
        }
    }

    #[test]
    fn workspace_patch_applies_exact_match_and_replace_all() {
        let root = TempRoot::new("patch");
        fs::write(root.0.join("code.rs"), "fn a() {}\nfn b() {}\nfn a() {}\n").expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        // Exact single match.
        let call = make_call(
            "c1",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"fn b() {}","new":"fn b() { patched }"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("replaced 1 occurrence"), "{summary}");
            }
            other => panic!("expected patch success, got {other:?}"),
        }
        let contents = fs::read_to_string(root.0.join("code.rs")).expect("read");
        assert!(contents.contains("fn b() { patched }"));

        // Ambiguous old text without replace_all is refused.
        let call = make_call(
            "c2",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"fn a() {}","new":"fn c() {}"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                let detail = detail.unwrap();
                assert!(detail.contains("2 locations"), "{detail}");
            }
            other => panic!("expected ambiguity refusal, got {other:?}"),
        }
        assert!(!fs::read_to_string(root.0.join("code.rs"))
            .expect("read")
            .contains("fn c()"));

        // replace_all replaces every occurrence.
        let call = make_call(
            "c3",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"fn a() {}","new":"fn c() {}","replace_all":true}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("replaced 2 occurrence"), "{summary}");
            }
            other => panic!("expected replace_all success, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(root.0.join("code.rs"))
                .expect("read")
                .matches("fn c() {}")
                .count(),
            2
        );

        // Absent old text is a handled model-visible failure.
        let call = make_call(
            "c4",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"not present","new":"x"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("not found"));
            }
            other => panic!("expected not-found failure, got {other:?}"),
        }
    }

    #[test]
    fn workspace_patch_refuses_malformed_arguments() {
        let root = TempRoot::new("patch-refuse");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        for arguments in [
            r#"{"path":"a.rs","old":"x","new":"x"}"#, // must differ
            r#"{"path":"a.rs","old":"","new":"y"}"#,  // empty old
            r#"{"path":"a.rs","old":"x","new":"y","replace_all":"yes"}"#,
            r#"{"path":"a.rs","old":"x"}"#,           // missing new
            r#"{"path":"../out.rs","old":"x","new":"y"}"#,
            r#"{"path":"a.rs","old":"x","new":"y","extra":1}"#,
        ] {
            let call = make_call("c1", WORKSPACE_PATCH_TOOL, arguments);
            let validated = tools.validate(&call, &cancel).expect("known tool validates");
            match tools.execute(&validated, &cancel).expect("handled") {
                ToolStepResult::Failed { handled, .. } => assert!(handled, "{arguments}"),
                other => panic!("expected handled refusal for {arguments}, got {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_exec_runs_argv_inside_the_root_with_bounded_output() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempRoot::new("shell");
        fs::write(root.0.join("echoer"), "#!/bin/sh\necho hello-out\necho hello-err >&2\n")
            .expect("seed");
        let chmod = |path: &Path| {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
        };
        chmod(&root.0.join("echoer"));
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = make_call(
            "c1",
            SHELL_EXEC_TOOL,
            r#"{"argv":["./echoer"],"timeout_ms":10000}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.starts_with("exit 0"), "{summary}");
                assert!(summary.contains("hello-out"), "{summary}");
                assert!(summary.contains("hello-err"), "{summary}");
            }
            other => panic!("expected shell success, got {other:?}"),
        }

        // The child runs with the workspace root as cwd.
        fs::write(root.0.join("pwd.sh"), "#!/bin/sh\npwd\n").expect("seed");
        chmod(&root.0.join("pwd.sh"));
        let call = make_call("c2", SHELL_EXEC_TOOL, r#"{"argv":["./pwd.sh"]}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains(root.0.to_string_lossy().as_ref()), "{summary}");
            }
            other => panic!("expected cwd proof, got {other:?}"),
        }

        // A failing command still yields a per-call outcome with its code.
        fs::write(root.0.join("fail.sh"), "#!/bin/sh\nexit 3\n").expect("seed");
        chmod(&root.0.join("fail.sh"));
        let call = make_call("c3", SHELL_EXEC_TOOL, r#"{"argv":["./fail.sh"]}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.starts_with("exit 3"), "{summary}");
            }
            other => panic!("expected exit-code outcome, got {other:?}"),
        }
    }

    #[test]
    fn shell_exec_enforces_its_timeout() {
        let root = TempRoot::new("shell-timeout");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = make_call(
            "c1",
            SHELL_EXEC_TOOL,
            r#"{"argv":["sleep","30"],"timeout_ms":300}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        let started = Instant::now();
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                let detail = detail.unwrap();
                assert!(detail.contains("timed out"), "{detail}");
                assert!(detail.contains("300ms"), "{detail}");
            }
            other => panic!("expected timeout failure, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the timeout must kill the child promptly"
        );
    }

    #[test]
    fn shell_exec_rejects_shell_strings_oversize_argv_and_bad_bounds() {
        let root = TempRoot::new("shell-refuse");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        // An explicit argv "sh -c script" is a valid argv form (the model may
        // invoke a shell binary directly); refusals here are shape/bound ones.
        let bad_arguments = [
            r#"{"argv":[]}"#,
            r#"{"argv":"echo hi"}"#,
            r#"{"argv":[42]}"#,
            r#"{"argv":["prog"],"timeout_ms":0}"#,
            r#"{"argv":["prog"],"timeout_ms":600001}"#,
            r#"{"argv":["prog"],"cwd":"/etc"}"#,
        ];
        let oversize = format!(r#"{{"argv":["{}"]}}"#, "x".repeat(MAX_SHELL_ARG_BYTES + 1));
        for arguments in bad_arguments.into_iter().chain(std::iter::once(oversize.as_str())) {
            let call = ProposedToolCall::new("c1", SHELL_EXEC_TOOL, arguments).expect("call");
            let validated = tools.validate(&call, &cancel).expect("known tool validates");
            match tools.execute(&validated, &cancel).expect("handled") {
                ToolStepResult::Failed { handled: true, .. } => {}
                other => panic!("expected handled refusal for {arguments}, got {other:?}"),
            }
        }
    }

    #[test]
    fn permission_gate_denies_calls_as_typed_model_visible_results() {
        let root = TempRoot::new("denied");
        fs::write(root.0.join("a.rs"), "x").expect("seed");
        let lattice = PermissionLattice::new(crate::permissions::PermissionMode::Default);
        let mut tools =
            ExecTools::workspace_with_permissions(&root.0, lattice).expect("tools");
        // File edits in default mode ask; headless exec renders that as a
        // typed denial with the reason, and nothing is written.
        let calls = vec![
            make_call(
                "c1",
                WORKSPACE_PATCH_TOOL,
                r#"{"path":"a.rs","old":"x","new":"y"}"#,
            ),
            make_call("c2", SHELL_EXEC_TOOL, r#"{"argv":["touch","a.rs"]}"#),
            // Read-only calls still auto-allow in default mode.
            make_call("c3", REPO_READ_TOOL, r#"{"path":"a.rs"}"#),
        ];
        let results = run_batch(&mut tools, &calls);
        assert!(matches!(
            results[0],
            Ok(ToolStepResult::Denied { ref detail, .. }) if detail.as_deref().unwrap_or("").contains("headless exec cannot ask")
        ));
        assert!(matches!(results[1], Ok(ToolStepResult::Denied { .. })));
        assert!(matches!(results[2], Ok(ToolStepResult::Succeeded { .. })));
        assert_eq!(
            fs::read_to_string(root.0.join("a.rs")).expect("read"),
            "x",
            "denied writes must not land"
        );
    }

    #[test]
    fn permission_deny_rule_beats_execution_even_for_reads() {
        let root = TempRoot::new("deny-rule");
        fs::write(root.0.join(".env.local"), "SECRET=1\n").expect("seed");
        let lattice = PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions)
            .with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("repo_read(.env*)").expect("rule"),
            }]);
        let mut tools =
            ExecTools::workspace_with_permissions(&root.0, lattice).expect("tools");
        let calls = vec![make_call("c1", REPO_READ_TOOL, r#"{"path":".env.local"}"#)];
        let results = run_batch(&mut tools, &calls);
        assert!(matches!(
            results[0],
            Ok(ToolStepResult::Denied { ref detail, .. }) if detail.as_deref().unwrap_or("").contains("deny rule")
        ));
    }

    #[test]
    fn persisted_grant_suppresses_the_ask_in_the_driver() {
        let root = TempRoot::new("grant");
        let lattice = PermissionLattice::new(crate::permissions::PermissionMode::Default)
            .with_grants(vec![ToolPattern::parse("workspace_patch").expect("grant")]);
        let mut tools =
            ExecTools::workspace_with_permissions(&root.0, lattice).expect("tools");
        fs::write(root.0.join("a.rs"), "x").expect("seed");
        let calls = vec![make_call(
            "c1",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"a.rs","old":"x","new":"y"}"#,
        )];
        let results = run_batch(&mut tools, &calls);
        assert!(matches!(results[0], Ok(ToolStepResult::Succeeded { .. })));
        assert_eq!(fs::read_to_string(root.0.join("a.rs")).expect("read"), "y");
    }

    #[test]
    fn batch_dispatch_runs_independent_calls_and_serializes_same_path_writes() {
        // Several independent calls plus two same-path writes in one step.
        // The writes serialize in proposal order: "1" then "2".
        let root = TempRoot::new("batch");
        fs::write(root.0.join("target.txt"), "base").expect("seed");
        for index in 0..3 {
            fs::write(root.0.join(format!("read{index}.txt")), format!("body {index}\n"))
                .expect("seed");
        }
        let mut tools = ExecTools::workspace_with_permissions(
            &root.0,
            PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions),
        )
        .expect("tools");
        let calls = vec![
            make_call("r0", REPO_READ_TOOL, r#"{"path":"read0.txt"}"#),
            make_call("w1", WORKSPACE_WRITE_TOOL, r#"{"path":"target.txt","content":"1"}"#),
            make_call("r1", REPO_READ_TOOL, r#"{"path":"read1.txt"}"#),
            make_call("w2", WORKSPACE_WRITE_TOOL, r#"{"path":"target.txt","content":"2"}"#),
            make_call("p1", WORKSPACE_PATCH_TOOL, r#"{"path":"target.txt","old":"2","new":"2-patched"}"#),
            make_call("r2", REPO_SEARCH_TOOL, r#"{"pattern":"body"}"#),
        ];
        let results = run_batch(&mut tools, &calls);
        // All calls complete with per-call results, in proposal order.
        assert_eq!(results.len(), 6);
        for (index, result) in results.iter().enumerate() {
            assert!(
                matches!(result, Ok(ToolStepResult::Succeeded { .. })),
                "call {index} must succeed, got {:?}",
                results[index]
            );
        }
        assert!(matches!(&results[0], Ok(ToolStepResult::Succeeded { summary, .. })
            if summary.contains("body 0")));
        assert!(matches!(&results[5], Ok(ToolStepResult::Succeeded { summary, .. })
            if summary.contains("body 1")));
        // Same-path writes applied in a serialized, defined order, then the patch.
        assert_eq!(
            fs::read_to_string(root.0.join("target.txt")).expect("read"),
            "2-patched"
        );
    }

    #[test]
    fn batch_same_path_patches_serialize_into_a_defined_order() {
        let root = TempRoot::new("batch-order");
        fs::write(root.0.join("log.txt"), "start").expect("seed");
        let mut tools = ExecTools::workspace_with_permissions(
            &root.0,
            PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions),
        )
        .expect("tools");
        let calls = vec![
            make_call("p1", WORKSPACE_PATCH_TOOL, r#"{"path":"log.txt","old":"start","new":"start-1"}"#),
            make_call("p2", WORKSPACE_PATCH_TOOL, r#"{"path":"log.txt","old":"start-1","new":"start-1-2"}"#),
        ];
        let results = run_batch(&mut tools, &calls);
        // Both succeed deterministically: p2 sees p1's output because
        // same-path writes serialize in proposal order.
        for result in &results {
            assert!(matches!(result, Ok(ToolStepResult::Succeeded { .. })));
        }
        assert_eq!(
            fs::read_to_string(root.0.join("log.txt")).expect("read"),
            "start-1-2"
        );
    }

    #[test]
    fn batch_shell_calls_serialize_with_each_other() {
        let root = TempRoot::new("batch-shell");
        let mut tools = ExecTools::workspace_with_permissions(
            &root.0,
            PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions),
        )
        .expect("tools");
        // Each shell call appends its own marker; serialization means both
        // markers land without racing the same file.
        let calls = vec![
            make_call("s1", SHELL_EXEC_TOOL, r#"{"argv":["touch","one.txt"]}"#),
            make_call("s2", SHELL_EXEC_TOOL, r#"{"argv":["touch","two.txt"]}"#),
        ];
        let results = run_batch(&mut tools, &calls);
        for result in &results {
            assert!(matches!(result, Ok(ToolStepResult::Succeeded { .. })), "{results:?}");
        }
        assert!(root.0.join("one.txt").exists());
        assert!(root.0.join("two.txt").exists());
    }

    #[test]
    fn independent_reads_execute_concurrently() {
        let root = TempRoot::new("concurrent");
        fs::write(root.0.join("a.txt"), "a").expect("seed");
        let workspace = WorkspaceTools::open(&root.0).expect("tools");
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Arc::new(Mutex::new(rx));
        let tools = ConcurrentProbeTools {
            workspace,
            tx,
            rx: Arc::clone(&rx),
        };
        let validated: Vec<ValidatedToolCall> = (0..2)
            .map(|index| {
                ValidatedToolCall::from_proposed(
                    &ProposedToolCall::new(
                        format!("c{index}"),
                        REPO_READ_TOOL,
                        r#"{"path":"a.txt"}"#,
                    )
                    .expect("call"),
                )
            })
            .collect();
        let outcomes = tools.execute_batch_public(&validated);
        for outcome in outcomes {
            assert!(matches!(outcome, Ok(ToolStepResult::Succeeded { .. })));
        }
    }

    /// Test-only wrapper proving the batch dispatch runs independent groups
    /// on threads: each call signals arrival, then waits for the peer's
    /// signal — only satisfiable when both run at once. A sequential dispatch
    /// would block until the rendezvous timeout and fail the assertion.
    struct ConcurrentProbeTools {
        workspace: WorkspaceTools,
        tx: mpsc::Sender<()>,
        rx: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    impl ConcurrentProbeTools {
        fn execute_batch_public(
            &self,
            calls: &[ValidatedToolCall],
        ) -> Vec<Result<ToolStepResult, ToolStepError>> {
            let workspace = &self.workspace;
            let tx = &self.tx;
            let rx = &self.rx;
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for call in calls {
                    let tx = tx.clone();
                    let rx = Arc::clone(rx);
                    let workspace: &WorkspaceTools = workspace;
                    handles.push(scope.spawn(move || {
                        let _ = tx.send(());
                        let _ = rx
                            .lock()
                            .expect("rx lock")
                            .recv_timeout(Duration::from_secs(10));
                        workspace.execute_call(call, &CancellationToken::new())
                    }));
                }
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("thread"))
                    .collect()
            })
        }
    }

    #[test]
    fn trusted_workspace_tools_complete_a_write_task_end_to_end() {
        // Drives the real exec entry (run_live_exec) with a scripted model
        // that proposes a file write and the real workspace tool driver.
        let root = TempRoot::new("e2e");
        let mut tools = ExecTools::workspace_with_permissions(
            &root.0,
            PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions),
        )
        .expect("tools");
        let mut events = Vec::new();
        let model = ScriptedModel::write_then_answer("notes/plan.md", "do the thing", "wrote it");
        let outcome = run_live_exec(
            preserved(),
            model,
            &exec_request(),
            &mut tools,
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.status(), AgentTerminalStatus::Succeeded);
        assert_eq!(outcome.result.summary(), "wrote it");
        assert_eq!(
            fs::read_to_string(root.0.join("notes/plan.md")).expect("file"),
            "do the thing"
        );
    }

    #[test]
    fn multi_tool_step_reaches_the_model_with_per_call_results_end_to_end() {
        // A full exec run proposing reads + a patch in one step; the turn
        // must complete with per-call outcomes and a patched file on disk.
        let root = TempRoot::new("e2e-multi");
        fs::write(root.0.join("app.txt"), "alpha beta").expect("seed");
        let mut tools = ExecTools::workspace_with_permissions(
            &root.0,
            PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions),
        )
        .expect("tools");
        let mut events = Vec::new();
        let model = ScriptedModel::calls_then_answer(
            vec![
                make_call("r1", REPO_READ_TOOL, r#"{"path":"app.txt"}"#),
                make_call(
                    "p1",
                    WORKSPACE_PATCH_TOOL,
                    r#"{"path":"app.txt","old":"beta","new":"gamma"}"#,
                ),
            ],
            "patched",
        );
        let outcome = run_live_exec(
            preserved(),
            model,
            &exec_request(),
            &mut tools,
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("execute");
        assert_eq!(outcome.result.status(), AgentTerminalStatus::Succeeded);
        assert_eq!(outcome.result.summary(), "patched");
        assert_eq!(
            fs::read_to_string(root.0.join("app.txt")).expect("read"),
            "alpha gamma"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind() == agent_runtime::TurnEventKind::ToolCompleted)
                .count(),
            2
        );
    }

    #[test]
    fn untrusted_surface_refuses_the_write_and_fails_closed() {
        let root = TempRoot::new("untrusted");
        let mut tools = ExecTools::noop();
        let mut events = Vec::new();
        let model = ScriptedModel::write_then_answer("plan.md", "do the thing", "unreachable");
        let outcome = run_live_exec(
            preserved(),
            model,
            &exec_request(),
            &mut tools,
            &mut events,
            &CancellationToken::new(),
            ContextRetryPolicy::new(2),
            None,
        )
        .expect("typed failed turn, not an execution error");
        assert_eq!(outcome.result.status(), AgentTerminalStatus::Failed);
        assert_eq!(
            outcome.failure_cause,
            None,
            "a tool refusal is not a provider failure"
        );
        assert!(!root.0.join("plan.md").exists(), "fail-closed: nothing written");
    }

    #[test]
    fn repo_glob_matches_patterns_and_caps_results() {
        let root = TempRoot::new("glob");
        fs::create_dir_all(root.0.join("src/deep")).expect("mkdir");
        fs::write(root.0.join("a.rs"), "a").expect("seed");
        fs::write(root.0.join("b.txt"), "b").expect("seed");
        fs::write(root.0.join("src/c.rs"), "c").expect("seed");
        fs::write(root.0.join("src/deep/d.rs"), "d").expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        // `*.rs` matches only the top level.
        let call = make_call("c1", REPO_GLOB_TOOL, r#"{"pattern":"*.rs"}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("a.rs"), "{summary}");
                assert!(!summary.contains("c.rs"), "star must not cross directories: {summary}");
            }
            other => panic!("expected glob success, got {other:?}"),
        }

        // `**/*.rs` matches at any depth, sorted, all three.
        let call = make_call("c2", REPO_GLOB_TOOL, r#"{"pattern":"**/*.rs"}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("a.rs") && summary.contains("c.rs") && summary.contains("d.rs"));
            }
            other => panic!("expected deep glob success, got {other:?}"),
        }

        // No match is a typed empty result; bad bounds are handled failures.
        let call = make_call("c3", REPO_GLOB_TOOL, r#"{"pattern":"*.zig"}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("no files match"));
            }
            other => panic!("expected empty glob, got {other:?}"),
        }
        let call = make_call("c4", REPO_GLOB_TOOL, r#"{"pattern":"*.rs","head_limit":0}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, .. } => assert!(handled),
            other => panic!("expected bound refusal, got {other:?}"),
        }
    }

    #[test]
    fn glob_path_match_semantics() {
        assert!(glob_path_match("*", "a.txt"));
        assert!(!glob_path_match("*", "src/a.txt"), "* stays in one segment");
        assert!(glob_path_match("**", "src/deep/a.rs"));
        assert!(glob_path_match("**/*.rs", "src/deep/a.rs"));
        assert!(glob_path_match("**/*.rs", "a.rs"), "star-star matches zero segments");
        assert!(glob_path_match("src/*.rs", "src/a.rs"));
        assert!(!glob_path_match("src/*.rs", "other/a.rs"));
        assert!(glob_path_match("src/?.rs", "src/a.rs"));
        assert!(!glob_path_match("src/?.rs", "src/ab.rs"));
    }

    #[test]
    fn todo_write_merges_by_id_and_persists() {
        let root = TempRoot::new("todo");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        // First write: two tasks without ids.
        let call = make_call(
            "c1",
            TODO_WRITE_TOOL,
            r#"{"todos":[{"content":"scan tests","status":"completed"},{"content":"fix bug","status":"in_progress"}]}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("2 task(s)"), "{summary}");
                assert!(summary.contains("→ fix bug"), "{summary}");
            }
            other => panic!("expected todo success, got {other:?}"),
        }

        // Second write: update by id and add a third task.
        let call = make_call(
            "c2",
            TODO_WRITE_TOOL,
            r#"{"todos":[{"id":"2","content":"fix bug","status":"completed"},{"content":"write docs","status":"pending"}]}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("3 task(s)"), "{summary}");
                assert!(summary.contains("0 in_progress"), "{summary}");
            }
            other => panic!("expected merge success, got {other:?}"),
        }
        let persisted = fs::read_to_string(root.0.join(TODOS_PATH)).expect("persisted");
        let value: serde_json::Value = serde_json::from_str(&persisted).expect("json");
        let todos = value["todos"].as_array().expect("todos array");
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[1]["id"], "2");
        assert_eq!(todos[1]["status"], "completed");

        // Unknown statuses and empty content are handled per-call failures.
        for arguments in [
            r#"{"todos":[{"content":"x","status":"done"}]}"#,
            r#"{"todos":[{"content":"","status":"pending"}]}"#,
            r#"{"todos":[]}"#,
            r#"{"items":[]}"#,
        ] {
            let call = make_call("c3", TODO_WRITE_TOOL, arguments);
            let validated = tools.validate(&call, &cancel).expect("known tool validates");
            match tools.execute(&validated, &cancel).expect("handled") {
                ToolStepResult::Failed { handled, .. } => assert!(handled, "{arguments}"),
                other => panic!("expected handled refusal for {arguments}, got {other:?}"),
            }
        }
    }

    #[test]
    fn every_tool_name_is_provider_portable() {
        // Providers disagree on tool-name alphabets: api.b.ai (and some other
        // OpenAI-compatible servers) enforce `^[a-zA-Z0-9_-]+$` and reject
        // dots, while OpenRouter tolerates them. Adopt the reference-CLI
        // practice (Claude Code, Grok Build): tool names use only
        // [a-zA-Z0-9_-], so one surface works with every provider.
        let portable = |name: &str| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        };
        let root = TempRoot::new("portable-names");
        for tool in ExecTools::workspace(&root.0).expect("tools").tool_surface() {
            assert!(
                portable(tool.name()),
                "tool name {:?} is not provider-portable (use [a-zA-Z0-9_-] only)",
                tool.name()
            );
        }
    }

    #[test]
    fn background_jobs_run_report_and_cancel() {
        let root = TempRoot::new("bg");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        // A fast job completes with its output spooled.
        let call = make_call(
            "c1",
            SHELL_EXEC_TOOL,
            r#"{"argv":["sh","-c","echo job-output-marker"],"background":true}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        let job_id = match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                let word = summary
                    .split_whitespace()
                    .find(|word| word.starts_with("job-"))
                    .expect("job id in summary");
                word.trim_end_matches(':').to_owned()
            }
            other => panic!("expected background start, got {other:?}"),
        };
        let mut completed = false;
        for _ in 0..50 {
            if let ToolStepResult::Succeeded { summary, .. } = {
                let status_call = make_call("s1", JOB_STATUS_TOOL, &format!(r#"{{"job_id":"{job_id}"}}"#));
                let validated = tools.validate(&status_call, &cancel).expect("validate");
                tools.execute(&validated, &cancel).expect("execute")
            } {
                if summary.contains("completed exit 0") {
                    completed = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(completed, "background job must complete");
        let output_call = make_call(
            "o1",
            JOB_OUTPUT_TOOL,
            &format!(r#"{{"job_id":"{job_id}","offset":0}}"#),
        );
        let validated = tools.validate(&output_call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("job-output-marker"), "{summary}");
                assert!(summary.contains("[job finished"), "{summary}");
            }
            other => panic!("expected output, got {other:?}"),
        }

        // A long job is observable as running, then cancelled at shutdown.
        let call = make_call(
            "c2",
            SHELL_EXEC_TOOL,
            r#"{"argv":["sleep","30"],"background":true}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        let long_id = match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                let word = summary
                    .split_whitespace()
                    .find(|word| word.starts_with("job-"))
                    .expect("job id in summary");
                word.trim_end_matches(':').to_owned()
            }
            other => panic!("expected background start, got {other:?}"),
        };
        {
            let status_call =
                make_call("s2", JOB_STATUS_TOOL, &format!(r#"{{"job_id":"{long_id}"}}"#));
            let validated = tools.validate(&status_call, &cancel).expect("validate");
            match tools.execute(&validated, &cancel).expect("execute") {
                ToolStepResult::Succeeded { summary, .. } => {
                    assert!(summary.contains("running"), "{summary}");
                }
                other => panic!("expected running, got {other:?}"),
            }
        }
        // Shutdown path: drop the registry clone to cancel + kill the child.
        drop(JobRegistry::default());
        let unknown = make_call("s3", JOB_STATUS_TOOL, r#"{"job_id":"job-missing"}"#);
        let validated = tools.validate(&unknown, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, .. } => assert!(handled),
            other => panic!("expected unknown-job failure, got {other:?}"),
        }
    }

    #[test]
    fn plan_mode_enforces_read_only_with_plan_file_carve_out() {
        let root = TempRoot::new("plan-mode");
        fs::write(root.0.join("code.rs"), "fn before() {}\n").expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        let enter = make_call("p1", PLAN_ENTER_TOOL, "{}");
        let validated = tools.validate(&enter, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("Plan mode active"), "{summary}");
            }
            other => panic!("expected enter success, got {other:?}"),
        }

        // Reads stay allowed; writes outside the plan file are denied with
        // the plan reason; writing the plan file itself is allowed.
        let read_call = make_call("r1", REPO_READ_TOOL, r#"{"path":"code.rs"}"#);
        let validated = tools.validate(&read_call, &cancel).expect("validate");
        assert!(matches!(
            tools.execute(&validated, &cancel).expect("execute"),
            ToolStepResult::Succeeded { .. }
        ));

        let patch = make_call(
            "p2",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"before","new":"after"}"#,
        );
        let validated = tools.validate(&patch, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Denied { detail, .. } => {
                assert!(detail.unwrap().contains("plan mode is read-only"));
            }
            other => panic!("expected plan denial, got {other:?}"),
        }
        assert!(
            fs::read_to_string(root.0.join("code.rs"))
                .expect("read")
                .contains("before"),
            "plan mode must keep the workspace intact"
        );

        let plan_write = make_call(
            "w1",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":".rapidlm/plan.md","content":"1. rename\n2. test"}"#,
        );
        let validated = tools.validate(&plan_write, &cancel).expect("validate");
        assert!(matches!(
            tools.execute(&validated, &cancel).expect("execute"),
            ToolStepResult::Succeeded { .. }
        ));

        // Exit reads the plan from disk and leaves plan mode.
        let exit = make_call("p3", PLAN_EXIT_TOOL, "{}");
        let validated = tools.validate(&exit, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("Plan accepted"), "{summary}");
                assert!(summary.contains("1. rename"));
            }
            other => panic!("expected exit success, got {other:?}"),
        }

        // Writes work again after exit.
        let patch = make_call(
            "p4",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"before","new":"after"}"#,
        );
        let validated = tools.validate(&patch, &cancel).expect("validate");
        assert!(matches!(
            tools.execute(&validated, &cancel).expect("execute"),
            ToolStepResult::Succeeded { .. }
        ));

        // Exiting while not in plan mode is a handled failure.
        let exit = make_call("p5", PLAN_EXIT_TOOL, "{}");
        let validated = tools.validate(&exit, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("not active"));
            }
            other => panic!("expected handled double-exit, got {other:?}"),
        }
    }

    #[test]
    fn task_spawn_uses_the_runner_and_enforces_depth_one() {
        use std::sync::Mutex as StdMutex;
        struct FakeRunner {
            calls: Arc<StdMutex<Vec<(String, String)>>>,
        }
        impl crate::exec_tools::SubagentRunner for FakeRunner {
            fn run(&self, prompt: &str, agent_type: &str) -> Result<String, String> {
                self.calls
                    .lock()
                    .expect("lock")
                    .push((prompt.to_owned(), agent_type.to_owned()));
                Ok("child finished the task".to_owned())
            }
        }

        let root = TempRoot::new("spawn");
        let mut tools = permissive_workspace(&root.0);
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let runner = Arc::new(FakeRunner {
            calls: Arc::clone(&calls),
        });
        tools.subagents = Some(runner as Arc<dyn SubagentRunner>);

        let call = make_call(
            "c1",
            TASK_SPAWN_TOOL,
            r#"{"prompt":"count the tests","type":"explore"}"#,
        );
        let validated = tools.validate(&call, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("e") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("subagent (explore)"), "{summary}");
                assert!(summary.contains("child finished the task"));
            }
            other => panic!("expected spawn success, got {other:?}"),
        }
        let recorded = calls.lock().expect("lock").clone();
        assert_eq!(
            recorded,
            vec![("count the tests".to_owned(), "explore".to_owned())]
        );

        // Unknown agent types are handled failures (never a dead turn).
        let bad = make_call(
            "c2",
            TASK_SPAWN_TOOL,
            r#"{"prompt":"x","type":"ninja"}"#,
        );
        let validated = tools.validate(&bad, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("handled") {
            ToolStepResult::Failed { handled, .. } => assert!(handled),
            other => panic!("expected handled type refusal, got {other:?}"),
        }

        // Read-only drivers never offer spawn tools: depth limit is structural.
        let child = WorkspaceTools::open_read_only(&root.0).expect("child");
        let surface_owned: Vec<String> = child
            .tool_surface()
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        let surface: Vec<&str> = surface_owned.iter().map(|name| name.as_str()).collect();
        assert!(!surface.contains(&TASK_SPAWN_TOOL), "depth 1 enforced: {surface:?}");
        assert!(surface.contains(&REPO_READ_TOOL), "reads stay available");
    }

    #[test]
    fn tool_surface_advertises_all_fourteen_tools_with_json_schemas() {
        let root = TempRoot::new("surface");
        let tools = ExecTools::workspace(&root.0).expect("tools");
        let surface = tools.tool_surface();
        let names: Vec<&str> = surface.iter().map(|tool| tool.name()).collect();
        assert_eq!(
            names,
            vec![
                WORKSPACE_WRITE_TOOL,
                WORKSPACE_READ_TOOL,
                REPO_READ_TOOL,
                REPO_SEARCH_TOOL,
                WORKSPACE_PATCH_TOOL,
                REPO_GLOB_TOOL,
                TODO_WRITE_TOOL,
                PLAN_ENTER_TOOL,
                PLAN_EXIT_TOOL,
                JOB_STATUS_TOOL,
                JOB_OUTPUT_TOOL,
                TASK_SPAWN_TOOL,
                SHELL_EXEC_TOOL,
            ]
        );
        for tool in &surface {
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object");
            assert!(schema["required"].is_array());
            assert_eq!(schema["additionalProperties"], false);
            assert!(!tool.description().is_empty());
        }
        // The untrusted surface advertises nothing.
        assert!(ExecTools::noop().tool_surface().is_empty());
    }

    #[test]
    fn classification_matches_the_tool_table() {
        assert_eq!(tool_kind(REPO_READ_TOOL), ToolKind::Read);
        assert_eq!(tool_kind(REPO_SEARCH_TOOL), ToolKind::Read);
        assert_eq!(tool_kind(WORKSPACE_READ_TOOL), ToolKind::Read);
        assert_eq!(tool_kind(WORKSPACE_WRITE_TOOL), ToolKind::Write);
        assert_eq!(tool_kind(WORKSPACE_PATCH_TOOL), ToolKind::Write);
        assert_eq!(tool_kind(SHELL_EXEC_TOOL), ToolKind::Write);
        assert_eq!(tool_kind(REPO_GLOB_TOOL), ToolKind::Read);
        assert_eq!(tool_kind(TODO_WRITE_TOOL), ToolKind::Write);
        assert_eq!(tool_kind(JOB_STATUS_TOOL), ToolKind::Read);
        assert_eq!(tool_kind(JOB_OUTPUT_TOOL), ToolKind::Read);
        assert_eq!(tool_kind(PLAN_ENTER_TOOL), ToolKind::Write);
        assert_eq!(tool_kind(TASK_SPAWN_TOOL), ToolKind::Write);
        assert_eq!(tool_kind("unknown"), ToolKind::Write, "unknown tools stay write-class");
    }
}
