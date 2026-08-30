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
    CancellationToken, ProposedToolCall, ToolDriver, ToolKind, ToolStepError, ToolStepExchange,
    ToolStepResult, ToolSurface, ValidatedToolCall,
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
/// Maximum `task_spawn` calls per turn (Modbit `WRK-017`'s concurrency
/// ceiling, narrowed to a total-per-turn cap — see `subagent_spawns`'s doc
/// comment on `WorkspaceTools` for why). A runaway loop that keeps spawning
/// subagents burns real tokens/cost/time with nothing else stopping it.
pub const MAX_SUBAGENT_SPAWNS_PER_TURN: u64 = 32;
/// Hard byte cap for the plan file.
pub const MAX_PLAN_BYTES: usize = 16 * 1024;
/// Cumulative byte cap on MCP-registered tools eagerly injected into the
/// advertised tool surface (Qwen Code's own cited number for the same
/// problem — see `newtask.md` §1.3/#9). A misconfigured or adversarial MCP
/// server can advertise arbitrarily many tools with arbitrarily large
/// schemas, and every one is re-sent on every model request for the rest
/// of the turn; this bounds the damage without building the fuller
/// lazy-hydration (`search_tool`/`use_tool` meta-tools) redesign.
pub const MAX_MCP_TOOL_SURFACE_BYTES: usize = 20 * 1024;
/// Adopted subagent types (the names both reference CLIs standardized on).
pub const AGENT_TYPES: &[&str] = &["general-purpose", "explore", "plan"];
/// Tool name for fetching a web page (Claude `WebFetch` parity).
pub const WEB_FETCH_TOOL: &str = "web_fetch";
/// Tool name for asking the user a question (Claude `AskUserQuestion` parity).
pub const ASK_USER_TOOL: &str = "ask_user";
/// Timeout for the ask_user stdin read.
pub const ASK_USER_TIMEOUT: Duration = Duration::from_secs(300);
/// Maximum options per ask_user question.
pub const MAX_ASK_USER_OPTIONS: usize = 8;
/// Hard byte cap on one tool call's JSON arguments.
pub const MAX_TOOL_ARGUMENTS_BYTES: usize = 8 * 1024;
/// Hard byte cap on a relative workspace path.
pub const MAX_TOOL_PATH_BYTES: usize = 512;
/// Hard byte cap on one file write.
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
/// Resource ceiling (Modbit `WRK-017`'s disk axis): cumulative bytes written
/// to disk across `workspace_write`/`workspace_patch` calls in one turn.
/// Per-call content is already bounded (`MAX_WRITE_BYTES`), but nothing
/// bounded the *count* of calls — a runaway loop writing max-size files
/// repeatedly could otherwise consume unbounded disk with no single call
/// ever exceeding its own cap. 1024x the per-call cap: generous enough for
/// any real coding task (scaffolding hundreds of files), tight enough to
/// stop a genuinely pathological loop.
pub const MAX_TOTAL_WRITE_BYTES_PER_TURN: u64 = 64 * 1024 * 1024;
/// Resource ceiling (Modbit `WRK-017`'s network axis): cumulative bytes
/// requested via `web_fetch` in one turn. Reserved against the call's own
/// `max_bytes` *before* the request goes out (a conservative worst-case,
/// not the actual response size, which isn't known until after the network
/// round trip) — the same "bound the call count, not just each call's own
/// size" shape as the disk-write budget above.
pub const MAX_TOTAL_FETCH_BYTES_PER_TURN: u64 = 16 * 1024 * 1024;
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
    reported: Arc<AtomicBool>,
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
            reported: Arc::new(AtomicBool::new(false)),
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
                let pipes: Vec<Box<dyn std::io::Read + Send>> = vec![
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

    /// Take every completed-but-unreported job as a model notification
    /// summary (job id, terminal state, bounded output). Running jobs stay
    /// pending; each job reports at most once.
    fn drain_notifications(&self) -> Vec<String> {
        let Ok(jobs) = self.jobs.lock() else {
            return Vec::new();
        };
        let mut notices = Vec::new();
        for (id, job) in jobs.iter() {
            if job.reported.load(Ordering::SeqCst) {
                continue;
            }
            let tail = {
                let Ok(state) = job.state.lock() else {
                    continue;
                };
                if matches!(*state, JobState::Running) {
                    continue;
                }
                job.reported.store(true, Ordering::SeqCst);
                let output = job.output.lock().ok();
                output
                    .as_ref()
                    .map(|buffer| {
                        let text = String::from_utf8_lossy(buffer);
                        let start = text.len().saturating_sub(512);
                        let mut start = start;
                        while start > 0 && !text.is_char_boundary(start) {
                            start -= 1;
                        }
                        text[start..].trim().to_owned()
                    })
                    .unwrap_or_default()
            };
            let state_text = {
                let Ok(state) = job.state.lock() else {
                    continue;
                };
                state.as_text().to_owned()
            };
            if tail.is_empty() {
                notices.push(format!("{id}: {state_text}"));
            } else {
                notices.push(format!("{id}: {state_text} — output: {tail}"));
            }
        }
        notices
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
    /// `write_scope`: confine the child's writes to this workspace-relative
    /// path or its descendants (Modbit `CAP-008`); `None` is unscoped.
    fn run(
        &self,
        prompt: &str,
        agent_type: &str,
        write_scope: Option<&str>,
    ) -> Result<SubagentReport, String>;
}

/// Structured result of one `task_spawn` child run. Kept typed across the
/// trait boundary instead of flattening to text inside the runner, so a test
/// double can assert on real fields rather than parsed prose, and any future
/// consumer (a ledger event, per-run cost accounting) has typed data instead
/// of needing to re-parse rendered text. Deliberately only carries fields
/// the runtime actually produces today (`ExecOutcome`, `apps/rapid/src/host.rs`)
/// — no invented "touched files"/"proposed patches" fields nothing populates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentReport {
    pub summary: String,
    pub status: String,
    pub tool_calls: u32,
    pub tokens: u64,
    /// Provider-reported dollar cost of the child's run, in micro-USD.
    /// `None` when no step reported one — see `ExecOutcome::cost_usd_micros`.
    pub cost_usd_micros: Option<u64>,
    pub stop_reason: Option<String>,
    /// `AgentResult::claims()`, pre-rendered as "text (result)" lines. Was
    /// silently dropped before this field existed even though the child's
    /// `AgentResult` already carried it (see `newtask.md` §2.2).
    pub claims: Vec<String>,
    /// `AgentResult::blockers()`, pre-rendered as "[kind] summary" lines.
    pub blockers: Vec<String>,
    /// `AgentResult::open_questions()`, verbatim.
    pub open_questions: Vec<String>,
    /// `AgentResult::patch_summary()`, pre-rendered as one line, when the
    /// child's turn included a workspace patch.
    pub patch_summary: Option<String>,
    /// `AgentResult::artifacts()`, pre-rendered as `"{id} ({media_type},
    /// {bytes}B, {redaction})"` lines. Safe to render uniformly across every
    /// `RedactionClass` including `Secret`: `protocol::ArtifactRef` is
    /// metadata-only (a content-addressed hash, a media type, a byte count,
    /// and the class itself) — none of those fields carry the artifact's
    /// actual content, so naming a secret artifact's reference is exactly
    /// the point of a reference architecture, not a leak of it. Was left
    /// unattempted when `claims`/`blockers`/etc. were added (see `newtask.md`
    /// §2.2) specifically pending this check.
    pub artifacts: Vec<String>,
}

/// Bounded tools rooted at one canonical workspace directory, with the
/// permission lattice that gates every call.
pub struct WorkspaceTools {
    root: PathBuf,
    permissions: PermissionLattice,
    jobs: JobRegistry,
    plan_mode: Arc<AtomicBool>,
    read_only: bool,
    /// One stderr line per tool call (name + outcome + detail). Off by
    /// default; headless `exec` turns it on so runs are diagnosable.
    trace_calls: bool,
    subagents: Option<Arc<dyn SubagentRunner>>,
    fetch_allowlist: Vec<String>,
    hooks: crate::hooks::HooksConfig,
    shadow_diagnostics: Option<crate::shadow_diagnostics::ShadowDiagnosticsConfig>,
    ask_stdin: Option<Arc<dyn Fn(&str, &[String], Duration) -> Result<String, String> + Send + Sync>>,
    mcp: Arc<Mutex<Vec<McpConnection>>>,
    mcp_surface: Arc<Mutex<Vec<(String, String, mcp::transport::McpToolDescriptor)>>>,
    /// Resource ceiling (Modbit `WRK-017`'s concurrency axis, narrowed to
    /// this codebase's actual shape: `task_spawn` runs synchronously, one
    /// subagent at a time, never several in parallel — so the real runaway
    /// risk is an unbounded *total* per turn, not concurrent execution).
    /// See `newtask.md` §2.10.
    subagent_spawns: Arc<AtomicU64>,
    /// Resource ceiling (Modbit `WRK-017`'s disk axis): cumulative bytes
    /// written via `workspace_write`/`workspace_patch` this turn — see
    /// `MAX_TOTAL_WRITE_BYTES_PER_TURN`'s own doc comment for why.
    bytes_written: Arc<AtomicU64>,
    /// Resource ceiling (Modbit `WRK-017`'s network axis): cumulative bytes
    /// requested via `web_fetch` this turn — see
    /// `MAX_TOTAL_FETCH_BYTES_PER_TURN`'s own doc comment for why.
    fetch_bytes: Arc<AtomicU64>,
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
            trace_calls: false,
            subagents: None,
            fetch_allowlist: Vec::new(),
            hooks: crate::hooks::HooksConfig::default(),
            shadow_diagnostics: None,
            ask_stdin: None,
            mcp: Arc::new(Mutex::new(Vec::new())),
            mcp_surface: Arc::new(Mutex::new(Vec::new())),
            subagent_spawns: Arc::new(AtomicU64::new(0)),
            bytes_written: Arc::new(AtomicU64::new(0)),
            fetch_bytes: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Emit one stderr line per tool call (name + one-line outcome + detail).
    /// Headless exec enables this; the interactive TUI keeps it off.
    pub fn set_trace_calls(&mut self, trace: bool) {
        self.trace_calls = trace;
    }

    /// Register configured stdio MCP servers: spawn, initialize, list tools,
    /// and record `mcp__<server>__<tool>` names on the surface. Servers that
    /// fail to start or handshake are recorded as offline (calls to them
    /// fail with a typed handled error) rather than skipped silently.
    pub fn register_mcp_servers(&mut self, servers: &[McpServerConfig]) {
        use mcp::transport::{
            ClientCapabilities, ImplementationInfo, IoBounds, McpSession, StdioTransport,
        };
        let bounds = IoBounds::new(64 * 1024, Duration::from_secs(30))
            .expect("standard io bounds");
        for server in servers {
            let spawn = std::process::Command::new(&server.command)
                .args(&server.args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn();
            let mut child = match spawn {
                Ok(child) => child,
                Err(_) => {
                    self.mcp_surface.lock().expect("mcp surface").push((
                        format!("mcp__{}__offline", server.name),
                        server.name.clone(),
                        mcp::transport::McpToolDescriptor {
                            name: "offline".to_owned(),
                            description: Some(format!(
                                "server {} failed to start",
                                server.name
                            )),
                            input_schema: serde_json::json!({}),
                        },
                    ));
                    self.mcp.lock().expect("mcp").push(McpConnection {
                        server: server.name.clone(),
                        online: false,
                        session: None,
                    });
                    continue;
                }
            };
            let stdout = child.stdout.take().expect("stdout piped");
            let stdin = child.stdin.take().expect("stdin piped");
            let mut session = McpSession::new(
                StdioTransport::from_pipes(stdout, stdin, None, bounds.clone()),
                ImplementationInfo::rapidlm(),
                ClientCapabilities::new(true),
            );
            let cancel = capability_broker::CancellationToken::new();
            let tools = match session.initialize(&cancel) {
                Ok(_) => match session.tools_list(&capability_broker::CancellationToken::new()) {
                    Ok(tools) => tools,
                    Err(_) => Vec::new(),
                },
                Err(_) => Vec::new(),
            };
            {
                let mut surface = self.mcp_surface.lock().expect("mcp surface");
                for tool in &tools {
                    surface.push((
                        format!("mcp__{}__{}", server.name, tool.name),
                        server.name.clone(),
                        tool.clone(),
                    ));
                }
            }
            self.mcp.lock().expect("mcp").push(McpConnection {
                server: server.name.clone(),
                online: true,
                session: Some(Mutex::new(session)),
            });
        }
    }

    /// Hosts web_fetch may fetch despite resolving private (local fixtures).
    pub fn set_fetch_allowlist(&mut self, allowlist: Vec<String>) {
        self.fetch_allowlist = allowlist;
    }

    /// Attach the shadow-diagnostics command: `workspace_write` calls whose
    /// path matches a configured glob are verified in an isolated Git
    /// worktree before ever reaching the real tree.
    pub fn set_shadow_diagnostics(
        &mut self,
        config: crate::shadow_diagnostics::ShadowDiagnosticsConfig,
    ) {
        self.shadow_diagnostics = Some(config);
    }

    /// Attach project hook commands (pre/post tool stages).
    pub fn set_hooks(&mut self, hooks: crate::hooks::HooksConfig) {
        self.hooks = hooks;
    }

    /// Attach the interactive answer source for ask_user. The closure
    /// receives the rendered prompt and the options; it returns the chosen
    /// option (composition root reads stdin and enforces the timeout).
    pub fn set_ask_source(
        &mut self,
        source: std::sync::Arc<
            dyn Fn(&str, &[String], Duration) -> Result<String, String> + Send + Sync,
        >,
    ) {
        self.ask_stdin = Some(source);
    }

    /// Read-only driver for subagent explore/plan scopes: write-classified
    /// tools are not advertised and any write attempt is refused.
    pub fn open_read_only(root: &Path) -> Result<Self, ToolSetupError> {
        Self::open_read_only_with_permissions(root, PermissionLattice::new(PermissionMode::Default))
    }

    /// Read-only driver with an explicit permission lattice: same structural
    /// write refusal as [`Self::open_read_only`], but deny/ask rules and
    /// persisted grants from the caller's lattice still apply to the reads
    /// that remain on the surface.
    pub fn open_read_only_with_permissions(
        root: &Path,
        permissions: PermissionLattice,
    ) -> Result<Self, ToolSetupError> {
        let mut tools = Self::open_with_permissions(root, permissions)?;
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
        // The parent check above only proves the containing directory sits
        // inside the workspace; a symlinked leaf (`ln -s /etc/passwd
        // leak.txt`) would still resolve outside the root on open/read/write.
        // `symlink_metadata` detects existence without following the link, so
        // a not-yet-created file (nothing to check) is left to the parent
        // check above.
        if target.symlink_metadata().is_ok() {
            let resolved = target.canonicalize().map_err(|_| ToolStepError::Invalid)?;
            if !resolved.starts_with(self.root()) {
                return Err(ToolStepError::Invalid);
            }
        }
        Ok(target)
    }

    /// Reserve `bytes` against the per-turn disk-write budget before
    /// actually writing. Atomic across concurrent write-classified calls on
    /// different paths (same-path writes already serialize via
    /// `write_group_key`): `fetch_add` first, then roll back with
    /// `fetch_sub` if that pushed the total over budget, so two concurrent
    /// near-the-limit writes can't both slip through a check-then-add race.
    /// `Some(detail)` when refused; `None` when the reservation succeeded.
    fn reserve_write_budget(&self, bytes: usize) -> Option<String> {
        let bytes = bytes as u64;
        let previous = self.bytes_written.fetch_add(bytes, Ordering::SeqCst);
        if previous.saturating_add(bytes) > MAX_TOTAL_WRITE_BYTES_PER_TURN {
            self.bytes_written.fetch_sub(bytes, Ordering::SeqCst);
            return Some(format!(
                "per-turn disk-write budget exhausted: {MAX_TOTAL_WRITE_BYTES_PER_TURN} bytes already written this turn"
            ));
        }
        None
    }

    /// Same shape as [`Self::reserve_write_budget`], for `web_fetch`'s
    /// network egress instead of disk writes.
    fn reserve_fetch_budget(&self, bytes: usize) -> Option<String> {
        let bytes = bytes as u64;
        let previous = self.fetch_bytes.fetch_add(bytes, Ordering::SeqCst);
        if previous.saturating_add(bytes) > MAX_TOTAL_FETCH_BYTES_PER_TURN {
            self.fetch_bytes.fetch_sub(bytes, Ordering::SeqCst);
            return Some(format!(
                "per-turn web_fetch budget exhausted: {MAX_TOTAL_FETCH_BYTES_PER_TURN} bytes already requested this turn"
            ));
        }
        None
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
            WEB_FETCH_TOOL => parse_web_fetch_args(arguments).ok().and_then(|(url, _)| {
                crate::web_fetch::host_of(&url).map(|host| format!("domain:{host}"))
            }),
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
        let outcome = self.execute_call_traced(call, cancel);
        if self.trace_calls {
            let line = match &outcome {
                Ok(ToolStepResult::Succeeded { summary, .. }) => {
                    format!("tool {}: ok ({})", call.tool(), single_line(summary))
                }
                Ok(ToolStepResult::Failed { detail, .. }) => format!(
                    "tool {}: failed ({})",
                    call.tool(),
                    single_line(detail.as_deref().unwrap_or("no detail"))
                ),
                Ok(ToolStepResult::Denied { detail, .. }) => format!(
                    "tool {}: denied ({})",
                    call.tool(),
                    single_line(detail.as_deref().unwrap_or("no detail"))
                ),
                Ok(ToolStepResult::ApprovalRequired { .. }) => {
                    format!("tool {}: approval_required", call.tool())
                }
                Err(err) => format!("tool {}: error ({})", call.tool(), err.as_str()),
            };
            crate::exec_diag::stderr_line(&bounded_detail(&line));
        }
        outcome
    }

    fn execute_call_traced(
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
        // never a dead turn. An unknown tool name (free-tier models propose
        // names from other CLIs, e.g. `read`) is the same: a handled failure
        // that names the mistake, not a structural refusal that ends the
        // turn.
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
        // Pre-tool-use hooks: the first denial wins and is model-visible.
        if !self.hooks.pre_tool_use.is_empty() {
            match crate::hooks::run_pre_tool_hooks(
                &self.hooks.pre_tool_use,
                call.tool(),
                call.arguments(),
                crate::hooks::HOOK_TIMEOUT,
            ) {
                crate::hooks::PreHookOutcome::Denied { reason } => {
                    return Ok(ToolStepResult::Denied {
                        call_id: call.call_id().to_owned(),
                        detail: Some(bounded_detail(&format!(
                            "{} blocked by pre_tool_use hook: {reason}",
                            call.tool()
                        ))),
                    });
                }
                crate::hooks::PreHookOutcome::Allowed => {}
            }
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
            WEB_FETCH_TOOL => parse_web_fetch_args(call.arguments()).is_ok(),
            ASK_USER_TOOL => parse_ask_user_args(call.arguments()).is_ok(),
            // Unknown tool names skip the shape pre-check: the dispatch
            // match below renders a handled "unknown tool" failure the
            // model can correct.
            _ => true,
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
        let result = match call.tool() {
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
            WEB_FETCH_TOOL => self.execute_web_fetch(call, cancel),
            ASK_USER_TOOL => self.execute_ask_user(call, cancel),
            other if other.starts_with("mcp__") => self.execute_mcp_tool(call, cancel),
            other => {
                // Name the valid tools inline rather than pointing back at
                // "the tool surface": a model that has already hallucinated
                // one name is the model most likely to do it again, and the
                // structured tool schemas sent with the request are easy to
                // lose track of turn over turn. Spelling the real names out
                // in the failure itself is the cheapest self-correction
                // signal available at the point it is needed.
                let surface = self.tool_surface();
                let mut names: Vec<&str> = surface.iter().map(ToolSurface::name).collect();
                names.sort_unstable();
                Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    // Kept compact against MAX_RESULT_DETAIL_BYTES (256): a
                    // wordy prefix once left the last few names (including
                    // workspace_read/workspace_write) truncated off the end.
                    detail: Some(bounded_detail(&format!(
                        "unknown tool `{other}`; real tools are: {}",
                        names.join(", ")
                    ))),
                })
            }
        }?;
        // Post-tool-use hooks observe the completed call; their output is
        // recorded on the result the model sees.
        if !self.hooks.post_tool_use.is_empty() {
            if let ToolStepResult::Succeeded { call_id, summary } = &result {
                let recorded = crate::hooks::run_post_tool_hooks(
                    &self.hooks.post_tool_use,
                    call.tool(),
                    summary,
                    crate::hooks::HOOK_TIMEOUT,
                );
                if !recorded.is_empty() {
                    return Ok(ToolStepResult::Succeeded {
                        call_id: call_id.clone(),
                        summary: bounded_detail(&format!(
                            "{summary}\n[post_tool_use: {recorded}]"
                        )),
                    });
                }
            }
        }
        Ok(result)
    }

    fn execute_write(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let args = parse_write_args(call.arguments())?;
        let target = self.resolve_in_root(&args.path)?;
        if let Some(detail) = self.reserve_write_budget(args.content.len()) {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&detail)),
            });
        }
        if is_team_memory_path(&args.path) {
            if let Some(note) =
                scan_for_secrets_advisory(self.root(), &args.path, args.content.as_bytes())
            {
                // .rapidlm/MEMORY.md is git-committed and team-shared (Modbit
                // row 12's `.qwen/team-memory/` parity): unlike an ordinary
                // write, a likely secret here is never advisory-only. Verify
                // before writing, same "never touch the real tree on a
                // failure" discipline shadow diagnostics uses. A dismissed
                // fingerprint (`rapid findings dismiss`) still unblocks —
                // that dismissal already represents a human decision that
                // it isn't a real secret.
                return Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&format!(
                        "write blocked: {} is git-committed, team-shared memory, where secret \
                         scanning is mandatory, not advisory; redact and retry, or dismiss a \
                         false positive first.\n{note}",
                        args.path
                    ))),
                });
            }
        }
        if let Some(config) = self
            .shadow_diagnostics
            .as_ref()
            .filter(|config| config.matches(&args.path, glob_path_match))
        {
            use crate::shadow_diagnostics::{ShadowVerifyOutcome, verify_candidate};
            match verify_candidate(self.root(), &args.path, args.content.as_bytes(), config) {
                ShadowVerifyOutcome::Failed { diagnostics_tail } => {
                    // Verify before showing, not after applying: the real
                    // tree is never touched by a candidate that fails
                    // diagnostics in isolation.
                    return Ok(ToolStepResult::Failed {
                        call_id: call.call_id().to_owned(),
                        handled: true,
                        detail: Some(bounded_detail(&format!(
                            "shadow diagnostics failed for {}; the write was NOT applied:\n{diagnostics_tail}",
                            args.path
                        ))),
                    });
                }
                ShadowVerifyOutcome::Passed { diagnostics_tail } => {
                    fs::write(&target, args.content.as_bytes()).map_err(|_| ToolStepError::Failed)?;
                    let mut summary = format!(
                        "wrote {} bytes to {} (shadow diagnostics: ok)\n{diagnostics_tail}",
                        args.content.len(),
                        args.path
                    );
                    append_write_advisories(&mut summary, self.root(), &args.path, args.content.as_bytes());
                    return Ok(ToolStepResult::Succeeded {
                        call_id: call.call_id().to_owned(),
                        summary: bounded_detail(&summary),
                    });
                }
                ShadowVerifyOutcome::Skipped { reason } => {
                    // Advisory-only: a broken/inapplicable shadow-diagnostics
                    // setup must never block a normal write. Falls through
                    // to the direct write below, noting the skip so it is
                    // not silently invisible.
                    fs::write(&target, args.content.as_bytes())
                        .map_err(|_| ToolStepError::Failed)?;
                    let mut summary = format!(
                        "wrote {} bytes to {} (shadow diagnostics skipped: {reason})",
                        args.content.len(),
                        args.path
                    );
                    append_write_advisories(&mut summary, self.root(), &args.path, args.content.as_bytes());
                    return Ok(ToolStepResult::Succeeded {
                        call_id: call.call_id().to_owned(),
                        summary: bounded_detail(&summary),
                    });
                }
            }
        }
        fs::write(&target, args.content.as_bytes()).map_err(|_| ToolStepError::Failed)?;
        let mut summary = format!("wrote {} bytes to {}", args.content.len(), args.path);
        append_write_advisories(&mut summary, self.root(), &args.path, args.content.as_bytes());
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary,
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
            Ok(bytes) => {
                // Rich reads: images become vision data URLs; PDFs are
                // text-extracted. Everything else stays bounded UTF-8 text.
                if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                    let dims = png_dimensions(&bytes);
                    use base64::Engine as _;
                    let data_url = format!(
                        "DATA_URL:data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    );
                    return Ok(ToolStepResult::Succeeded {
                        call_id: call.call_id().to_owned(),
                        summary: bounded_detail(&format!(
                            "PNG image{}; {} bytes; inline vision content:\n{}",
                            dims.map(|(w, h)| format!(" {w}x{h}")).unwrap_or_default(),
                            bytes.len(),
                            data_url
                        )),
                    });
                }
                if bytes.starts_with(b"%PDF-") {
                    let extracted = crate::pdf_text::extract_pdf_text(&bytes);
                    let pages = pdf_page_count(&bytes);
                    return Ok(ToolStepResult::Succeeded {
                        call_id: call.call_id().to_owned(),
                        summary: bounded_detail(&match extracted {
                            Some(text) => format!(
                                "PDF, {pages} page(s); extracted text:\n{}",
                                bounded_text(text.as_bytes(), MAX_READ_BYTES)
                            ),
                            None => format!(
                                "PDF, {pages} page(s); no extractable text (scanned or \
                                 encoded content)"
                            ),
                        }),
                    });
                }
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary: bounded_text(&bytes, MAX_READ_BYTES),
                })
            }
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
        let exact_occurrences = contents.matches(&args.old).count();
        if exact_occurrences > 0 {
            if exact_occurrences > 1 && !args.replace_all {
                return Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&format!(
                        "{}: old text matches {exact_occurrences} locations; expand old text or set replace_all",
                        args.path
                    ))),
                });
            }
            let updated = contents.replace(&args.old, &args.new);
            if let Some(detail) = self.reserve_write_budget(updated.len()) {
                return Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&detail)),
                });
            }
            fs::write(&target, updated.as_bytes()).map_err(|_| ToolStepError::Failed)?;
            let mut summary = format!("replaced {exact_occurrences} occurrence(s) in {}", args.path);
            append_write_advisories(&mut summary, self.root(), &args.path, updated.as_bytes());
            return Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary,
            });
        }
        // Second tier: the exact substring wasn't found, but the same lines
        // may exist with different indentation/spacing (a very common model
        // mistake — reindented or reflowed `old` text). Never re-flows the
        // replacement: `new` is spliced in exactly as given, only the *match*
        // is whitespace-tolerant.
        let loose_matches = find_whitespace_insensitive(&contents, &args.old);
        if loose_matches.is_empty() {
            let mut detail = format!("{}: old text not found", args.path);
            if let Some((line_no, line_text)) = suggest_closest_line(&contents, &args.old) {
                detail.push_str(&format!("; closest existing line {line_no}: {line_text}"));
            }
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&detail)),
            });
        }
        if loose_matches.len() > 1 && !args.replace_all {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "{}: old text matches {} locations after ignoring whitespace; expand old text or set replace_all",
                    args.path,
                    loose_matches.len()
                ))),
            });
        }
        let selected = if args.replace_all {
            loose_matches.as_slice()
        } else {
            &loose_matches[..1]
        };
        let mut updated = contents.clone();
        for range in selected.iter().rev() {
            updated.replace_range(range.clone(), &args.new);
        }
        if let Some(detail) = self.reserve_write_budget(updated.len()) {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&detail)),
            });
        }
        fs::write(&target, updated.as_bytes()).map_err(|_| ToolStepError::Failed)?;
        let mut summary = format!(
            "replaced {} occurrence(s) in {} (whitespace-insensitive match)",
            selected.len(),
            args.path
        );
        append_write_advisories(&mut summary, self.root(), &args.path, updated.as_bytes());
        Ok(ToolStepResult::Succeeded {
            call_id: call.call_id().to_owned(),
            summary,
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
        if self.trace_calls {
            crate::exec_diag::stderr_line(&format!(
                "tool shell_exec: argv={:?} background={} sandbox={}", args.argv, args.background, args.sandbox
            ));
        }
        if args.sandbox {
            // Seatbelt confinement (macOS): workspace writes allowed, other
            // writes denied. Runs as an async background job — see
            // sandbox_exec's own doc comment for why the two paths aren't
            // unified yet.
            if let Some(sandbox_exec) = find_sandbox_exec() {
                let profile_path = self
                    .resolve_in_root(".rapidlm/seatbelt.sb")
                    .map_err(|_| ToolStepError::Failed)?;
                fs::write(
                    &profile_path,
                    seatbelt_profile(self.root()).as_bytes(),
                )
                .map_err(|_| ToolStepError::Failed)?;
                let mut sandboxed = vec![sandbox_exec.to_string_lossy().into_owned()];
                sandboxed.push("-f".to_owned());
                sandboxed.push(profile_path.to_string_lossy().into_owned());
                sandboxed.extend(args.argv.iter().cloned());
                let job_id = self.jobs.start(&sandboxed, self.root(), args.timeout)?;
                let mut summary = format!(
                    "started sandboxed job {job_id}: {} (timeout {}s); poll with job_status",
                    sandboxed[3..].join(" "),
                    args.timeout.as_secs()
                );
                if let Some(note) = scan_command_advisory(self.root(), &args.argv) {
                    summary.push('\n');
                    summary.push_str(&note);
                }
                return Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary,
                });
            }
            // Non-macOS (or sandbox-exec missing): the tiered `sandbox`
            // crate's host-restricted backend — real process-group isolation
            // plus CPU/memory/pid limits, synchronous (unlike the job above;
            // see sandbox_exec's doc comment). A genuine capability where
            // this previously just failed outright.
            return match crate::sandbox_exec::run_sandboxed(
                self.root(),
                &args.argv,
                args.timeout,
                MAX_SHELL_OUTPUT_BYTES as u64,
            ) {
                Ok(outcome) => {
                    let output = bounded_text(&outcome.output, MAX_SHELL_OUTPUT_BYTES);
                    let status = match outcome.exit_code {
                        Some(code) => format!("exit {code}"),
                        None if outcome.timed_out => "timed out".to_owned(),
                        None => "no exit code (signalled)".to_owned(),
                    };
                    let mut summary = format!("sandboxed {status}\n{output}");
                    if let Some(note) = scan_command_advisory(self.root(), &args.argv) {
                        summary.push('\n');
                        summary.push_str(&note);
                    }
                    Ok(ToolStepResult::Succeeded {
                        call_id: call.call_id().to_owned(),
                        summary,
                    })
                }
                Err(err) => Ok(ToolStepResult::Failed {
                    call_id: call.call_id().to_owned(),
                    handled: true,
                    detail: Some(bounded_detail(&format!("sandboxed exec failed: {err}"))),
                }),
            };
        }
        if args.background {
            let job_id = self.jobs.start(&args.argv, self.root(), args.timeout)?;
            let mut summary = format!(
                "started background job {job_id}: {} (timeout {}s); poll with job_status / read with job_output",
                args.argv.join(" "),
                args.timeout.as_secs()
            );
            if let Some(note) = scan_command_advisory(self.root(), &args.argv) {
                summary.push('\n');
                summary.push_str(&note);
            }
            return Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary,
            });
        }
        let command_advisory = scan_command_advisory(self.root(), &args.argv);
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
                let mut summary = format!("exit {code}\n{output_text}");
                if let Some(note) = command_advisory {
                    summary.push('\n');
                    summary.push_str(&note);
                }
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary,
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

    /// `ask_user`: surface a question with options; the selected option is
    /// read from the configured stdin source. Headless runs (no source)
    /// return a typed refusal so the model can proceed on judgment.
    fn execute_ask_user(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let (question, options) = parse_ask_user_args(call.arguments())?;
        let Some(ask) = self.ask_stdin.as_deref() else {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(
                    "ask_user requires an interactive user; none is available in this \
                     headless run — proceed with best judgment and state assumptions",
                )),
            });
        };
        let listing: String = options
            .iter()
            .enumerate()
            .map(|(index, option)| format!("{}. {option}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!("{question}\n{listing}\nAnswer with the option number: ");
        match ask(&prompt, &options, ASK_USER_TIMEOUT) {
            Ok(chosen) => Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: format!("user selected: {chosen}"),
            }),
            Err(reason) => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&reason)),
            }),
        }
    }

    /// `mcp__<server>__<tool>`: dispatch through the MCP JSON-RPC session.
    fn execute_mcp_tool(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        const MCP_RESULT_CAP: usize = 20 * 1024;
        let wire = call.tool();
        let rest = wire.strip_prefix("mcp__").unwrap_or(wire);
        let Some((server_name, tool_name)) = rest.split_once("__") else {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "{wire}: malformed MCP tool name (expected mcp__<server>__<tool>)"
                ))),
            });
        };
        let arguments: serde_json::Value = serde_json::from_str(call.arguments())
            .unwrap_or_else(|_| serde_json::json!({}));
        let Ok(connections) = self.mcp.lock() else {
            return Err(ToolStepError::Failed);
        };
        let Some(connection) = connections
            .iter()
            .find(|connection| connection.server == server_name)
        else {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "MCP server {server_name:?} is not configured"
                ))),
            });
        };
        if !connection.online {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "MCP server {server_name:?} failed to start"
                ))),
            });
        }
        let Some(session_ref) = connection.session.as_ref() else {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "MCP server {server_name:?} is offline"
                ))),
            });
        };
        let mut session = match session_ref.lock() {
            Ok(session) => session,
            Err(_) => return Err(ToolStepError::Failed),
        };
        let cancel = capability_broker::CancellationToken::new();
        let watchdog = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(30));
                cancel.cancel();
            })
        };
        let outcome = session.tools_call(tool_name, &arguments, &cancel);
        drop(watchdog);
        match outcome {
            Ok(output) if !output.is_error => Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: bounded_detail(&format!(
                    "[mcp:{server_name}]\n{}",
                    crate::exec_tools::truncate_str(&output.text, MCP_RESULT_CAP)
                )),
            }),
            Ok(output) => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "[mcp:{server_name}] tool error: {}",
                    output.text
                ))),
            }),
            Err(err) => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "[mcp:{server_name}] {err}"
                ))),
            }),
        }
    }

    /// `web_fetch`: SSRF-guarded page fetch, HTML stripped to bounded text.
    fn execute_web_fetch(
        &self,
        call: &ValidatedToolCall,
        _cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        let (url, max_bytes) = parse_web_fetch_args(call.arguments())?;
        if let Some(detail) = self.reserve_fetch_budget(max_bytes) {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&detail)),
            });
        }
        match crate::web_fetch::fetch_page(&url, &self.fetch_allowlist, max_bytes) {
            Ok(text) if text.is_empty() => Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: format!("fetched {url}: empty page"),
            }),
            Ok(text) => Ok(ToolStepResult::Succeeded {
                call_id: call.call_id().to_owned(),
                summary: bounded_detail(&format!("fetched {url}:\n{text}")),
            }),
            Err(refusal) => Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "web_fetch refused: {}",
                    refusal.detail()
                ))),
            }),
        }
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
        if self.subagent_spawns.fetch_add(1, Ordering::SeqCst) >= MAX_SUBAGENT_SPAWNS_PER_TURN {
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some(bounded_detail(&format!(
                    "task_spawn budget exhausted: {MAX_SUBAGENT_SPAWNS_PER_TURN} subagents already started this turn"
                ))),
            });
        }
        if !self.hooks.subagent_start.is_empty() {
            let _ = crate::hooks::run_notify_hooks(
                &self.hooks.subagent_start,
                "subagent_start",
                serde_json::json!({"agent_type": args.agent_type}),
                crate::hooks::HOOK_TIMEOUT,
            );
        }
        let outcome = runner.run(&args.prompt, &args.agent_type, args.write_scope.as_deref());
        if !self.hooks.subagent_stop.is_empty() {
            let (status, ok) = match &outcome {
                Ok(report) => (report.status.clone(), true),
                Err(_) => ("failed".to_owned(), false),
            };
            let _ = crate::hooks::run_notify_hooks(
                &self.hooks.subagent_stop,
                "subagent_stop",
                serde_json::json!({"agent_type": args.agent_type, "status": status, "ok": ok}),
                crate::hooks::HOOK_TIMEOUT,
            );
        }
        match outcome {
            Ok(report) => {
                let body = bounded_text(report.summary.as_bytes(), MAX_SUBAGENT_REPORT_BYTES);
                let mut header = format!(
                    "subagent ({}) report [status={} tool_calls={} tokens={}",
                    args.agent_type, report.status, report.tool_calls, report.tokens
                );
                if let Some(cost_usd_micros) = report.cost_usd_micros {
                    header.push_str(&format!(" cost_usd_micros={cost_usd_micros}"));
                }
                if let Some(reason) = &report.stop_reason {
                    header.push_str(&format!(" stop_reason={reason}"));
                }
                header.push(']');
                let mut summary = format!("{header}:\n{body}");
                if let Some(patch_summary) = &report.patch_summary {
                    summary.push_str(&format!("\npatch: {patch_summary}"));
                }
                for claim in &report.claims {
                    summary.push_str(&format!("\nclaim: {claim}"));
                }
                for blocker in &report.blockers {
                    summary.push_str(&format!("\nblocker: {blocker}"));
                }
                for question in &report.open_questions {
                    summary.push_str(&format!("\nopen question: {question}"));
                }
                for artifact in &report.artifacts {
                    summary.push_str(&format!("\nartifact: {artifact}"));
                }
                Ok(ToolStepResult::Succeeded {
                    call_id: call.call_id().to_owned(),
                    summary,
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
    /// `index` is the call's position in the batch, used to give every
    /// uncategorized write (task_spawn, ask_user, plan_enter/exit, any
    /// mcp__* tool) its own group: those calls target no shared resource, so
    /// they must run concurrently rather than collapsing onto one `None` key
    /// and serializing behind each other.
    fn write_group_key(call: &ValidatedToolCall, index: usize) -> Option<String> {
        match call.tool() {
            SHELL_EXEC_TOOL => Some(SHELL_EXEC_TOOL.to_owned()),
            WORKSPACE_WRITE_TOOL => {
                parse_write_args(call.arguments()).ok().map(|args| args.path)
            }
            WORKSPACE_PATCH_TOOL => parse_patch_args(call.arguments()).ok().map(|a| a.path),
            TODO_WRITE_TOOL => Some(TODOS_PATH.to_owned()),
            _ => Some(format!("solo:{index}")),
        }
    }
}

/// Read/write classification the parallel dispatcher uses. Read-only tools
/// run concurrently with everything; writes serialize per target.
pub fn tool_kind(tool: &str) -> ToolKind {
    match tool {
        WORKSPACE_READ_TOOL | REPO_READ_TOOL | REPO_SEARCH_TOOL | REPO_GLOB_TOOL
        | JOB_STATUS_TOOL | JOB_OUTPUT_TOOL | WEB_FETCH_TOOL => ToolKind::Read,
        _ => ToolKind::Write,
    }
}

fn tool_class(tool: &str) -> ToolClass {
    match tool {
        WORKSPACE_READ_TOOL | REPO_READ_TOOL | REPO_SEARCH_TOOL | REPO_GLOB_TOOL
        | JOB_STATUS_TOOL | JOB_OUTPUT_TOOL | PLAN_ENTER_TOOL | PLAN_EXIT_TOOL
        | WEB_FETCH_TOOL => ToolClass::ReadOnly,
        WORKSPACE_WRITE_TOOL | WORKSPACE_PATCH_TOOL | TODO_WRITE_TOOL => ToolClass::FileEdit,
        _ => ToolClass::Other,
    }
}

/// PNG dimensions from the IHDR chunk (0,0 when malformed).
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

/// Count `/Type /Page` objects (not /Pages) as a page estimate.
fn pdf_page_count(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut cursor = 0;
    while let Some(rel) = find_bytes(&bytes[cursor..], b"/Type") {
        let at = cursor + rel;
        let rest = &bytes[at + 6..];
        let skip = rest.iter().take(4).count();
        let _ = skip;
        let trimmed = leading_spaces(rest);
        if rest[trimmed..].starts_with(b"/Page")
            && !rest[trimmed..].starts_with(b"/Pages")
        {
            count += 1;
        }
        cursor = at + 6;
    }
    count.max(1)
}

fn leading_spaces(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| **byte == b' ' || **byte == b'\n' || **byte == b'\r' || **byte == b'\t')
        .count()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
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
/// The one git-committed, team-shared memory file this codebase has today
/// (`apps/rapid/src/host.rs::load_memory_index`) — narrower than Qwen's
/// `.qwen/team-memory/` directory tier (Modbit row 12), but the part that
/// makes `workspace_write`'s secret-scan gate here mandatory rather than
/// advisory (see the call site in `execute_write`).
fn is_team_memory_path(path: &str) -> bool {
    path == ".rapidlm/MEMORY.md"
}

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

/// Strip raw control bytes that survive JSON serialization unescaped (DEL
/// 0x7F and the C1 control range 0x80-0x9F; serde_json only escapes
/// 0x00-0x1F) and would otherwise trip `ProposedToolCall`'s
/// no-control-chars validation downstream. `\n`, `\r`, `\t` are kept:
/// serde_json escapes those into safe non-control sequences.
fn sanitize_notification_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .collect()
}

/// Flatten a detail onto one line so trace output stays line-oriented.
fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
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
    sandbox: bool,
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
    /// Optional workspace-relative path the child may write inside, never
    /// outside (Modbit `CAP-008`: a narrow write scope). `None`: unscoped,
    /// today's existing behavior.
    write_scope: Option<String>,
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

/// Every advisory-only content scan a successful write/patch can trigger,
/// in one place: secrets, patch-policy, and Next-Edit-Ripple. Every write
/// path in this file (`execute_write`'s plain and shadow-diagnostics
/// branches, both `execute_patch` match tiers) funnels through this rather
/// than repeating the same three calls, so a future fourth scanner has one
/// call site to add, not five.
fn append_write_advisories(summary: &mut String, root: &Path, path: &str, content: &[u8]) {
    if let Some(note) = scan_for_secrets_advisory(root, path, content) {
        summary.push('\n');
        summary.push_str(&note);
    }
    if let Some(note) = scan_patch_advisory(root, path, content) {
        summary.push('\n');
        summary.push_str(&note);
    }
    if let Some(note) = crate::context_retrieval::ripple_advisory(root, path) {
        summary.push('\n');
        summary.push_str(&note);
    }
}

/// Advisory-only secret scan of newly-written content (Modbit `VER-007`/
/// `VER-008`'s `Finding` model, already built in `security::scanners::secrets`
/// but with zero call sites anywhere in `apps/rapid` before this — see
/// `newtask.md` §2.9). Never blocks the write: a scanner false positive
/// (e.g. a high-entropy test fixture) must never break a legitimate
/// workflow, so this only appends a note to the tool's own success summary
/// — the model sees it and can redact/rewrite if the flag is real, the same
/// model-correctable-not-fatal shape every other advisory in this codebase
/// uses. `None` on a scan failure (bad path, oversized content) or no
/// (non-dismissed) findings; failures are silent since this is advisory,
/// not a gate. Findings already dismissed via `rapid findings dismiss`
/// (`crate::findings_store::FindingsStore`, keyed by content-hash
/// fingerprint) never resurface on a rerun — but a *changed* finding at the
/// same location gets a different fingerprint and is never silently hidden.
fn scan_for_secrets_advisory(root: &Path, path: &str, content: &[u8]) -> Option<String> {
    let repo_path = protocol::RepoPath::parse(path).ok()?;
    let target = security::ScanTarget::staged_diff(repo_path, content.to_vec()).ok()?;
    let mut request = security::ScanRequest::new();
    request.push_target(target).ok()?;
    let scanner = security::SecretScanner::new();
    let cancel = security::ScanCancellation::new();
    let report = scanner.scan(&request, &cancel).ok()?;
    let store = crate::findings_store::FindingsStore::load(root);
    let findings: Vec<&security::Finding> = report
        .findings()
        .iter()
        .filter(|finding| !store.is_dismissed(finding.fingerprint().as_hex()))
        .collect();
    if findings.is_empty() {
        return None;
    }
    let details: Vec<String> = findings
        .iter()
        .map(|finding| format!("{} ({})", finding.rule_id(), finding.fingerprint().as_hex()))
        .collect();
    Some(format!(
        "advisory: possible secrets detected: {} — verify before committing, or dismiss a \
         false positive with `rapid findings dismiss <fingerprint>`",
        details.join(", ")
    ))
}

/// `capability_broker::Resolver` for a path already made absolute by the
/// caller (`resolve_program`, below) — mirrors `p9_commands.rs`'s
/// `FrozenPathResolver` exactly (a trivial, always-available impl duplicated
/// rather than shared across modules for two callers this small).
struct AlreadyResolvedPathResolver;

impl capability_broker::Resolver for AlreadyResolvedPathResolver {
    fn resolve_cwd(
        &self,
        requested: &str,
    ) -> Result<capability_broker::CanonicalHostPath, capability_broker::CommandNormalizeError> {
        capability_broker::CanonicalHostPath::from_resolved(requested)
    }

    fn resolve_executable(
        &self,
        requested: &str,
        _cwd: &capability_broker::CanonicalHostPath,
    ) -> Result<capability_broker::CanonicalHostPath, capability_broker::CommandNormalizeError> {
        capability_broker::CanonicalHostPath::from_resolved(requested)
            .map_err(|_| capability_broker::CommandNormalizeError::UnresolvedExecutable)
    }
}

/// Advisory-only dangerous-command scan of a `shell_exec` call (Modbit
/// `VER-007`'s `CommandFinding` scanner — real, mature, built, with zero
/// call sites anywhere in `apps/rapid` before this; see `newtask.md` §2.9's
/// note on why this needed a real `Resolver`/`normalize_exec` ceremony,
/// unlike the secrets scanner's simpler path). Same shape throughout:
/// `argv[0]` is resolved to an absolute path via `sandbox_exec::
/// resolve_program` (the same `$PATH`/root-relative resolution the
/// non-macOS sandbox path already uses) purely so the scanner has a
/// well-formed `CanonicalCommand` to classify — this never runs the
/// command, never gates it, and a resolution failure is silently `None`,
/// not a refusal. Findings are dismissible via the same
/// `FindingsStore` every other scanner in this file uses (fingerprint-hex
/// keyed, not scanner-specific).
fn scan_command_advisory(root: &Path, argv: &[String]) -> Option<String> {
    let mut resolved_argv = argv.to_vec();
    resolved_argv[0] = crate::sandbox_exec::resolve_program(root, argv.first()?).ok()?;
    let root_str = root.to_str()?;
    let intent = capability_broker::ExecIntent::argv(resolved_argv, root_str, Vec::<String>::new());
    let cancel = capability_broker::CancellationToken::new();
    let command =
        capability_broker::normalize_exec(&intent, &AlreadyResolvedPathResolver, &cancel).ok()?;
    let scanner = security::CommandRiskScanner::new();
    let scan_cancel = security::CommandScanCancellation::new();
    let report = scanner.scan(&command, &scan_cancel).ok()?;
    let store = crate::findings_store::FindingsStore::load(root);
    let findings: Vec<&security::CommandFinding> = report
        .findings()
        .iter()
        .filter(|finding| !store.is_dismissed(finding.fingerprint().as_hex()))
        .collect();
    if findings.is_empty() {
        return None;
    }
    let details: Vec<String> = findings
        .iter()
        .map(|finding| format!("{} ({})", finding.rule_id(), finding.fingerprint().as_hex()))
        .collect();
    Some(format!(
        "advisory: possible dangerous command detected: {} — verify before running, or dismiss a \
         false positive with `rapid findings dismiss <fingerprint>`",
        details.join(", ")
    ))
}

/// Advisory-only staged-patch scan of newly-written content (Modbit
/// `VER-007`'s `PatchFinding` scanner — permission broadening, credential
/// handling, CI/release changes, executable hooks — real, mature, built,
/// with zero call sites anywhere in `apps/rapid` before this). Unlike
/// `CommandFinding`, `PatchScanTarget::create` needed no `Resolver`/
/// `normalize_exec` ceremony — it takes a `protocol::RepoPath` directly,
/// which `args.path` already satisfies (workspace-relative, no `..`,
/// already validated by `checked_relative` before this runs). Always
/// scanned as `Create` with `executable: false`: this write path has no
/// chmod capability, so a target it produces is never actually executable,
/// and the content-scanning rules (credentials, CI/release paths, sudoers)
/// that can fire here don't distinguish `Create` from `Replace` — only
/// `Delete`/`Move`, which this path never produces. Same dismiss/never-
/// block shape as every other scanner in this file.
fn scan_patch_advisory(root: &Path, path: &str, content: &[u8]) -> Option<String> {
    let repo_path = protocol::RepoPath::parse(path).ok()?;
    let target = security::PatchScanTarget::create(repo_path, content.to_vec(), false).ok()?;
    let mut request = security::PatchScanRequest::new();
    request.push_target(target).ok()?;
    let scanner = security::PatchScanner::new();
    let cancel = security::PatchScanCancellation::new();
    let report = scanner.scan(&request, &cancel).ok()?;
    let store = crate::findings_store::FindingsStore::load(root);
    let findings: Vec<&security::PatchFinding> = report
        .findings()
        .iter()
        .filter(|finding| !store.is_dismissed(finding.fingerprint().as_hex()))
        .collect();
    if findings.is_empty() {
        return None;
    }
    let details: Vec<String> = findings
        .iter()
        .map(|finding| format!("{} ({})", finding.rule_id(), finding.fingerprint().as_hex()))
        .collect();
    Some(format!(
        "advisory: possible patch-policy issue detected: {} — verify before committing, or \
         dismiss a false positive with `rapid findings dismiss <fingerprint>`",
        details.join(", ")
    ))
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

/// Byte ranges of each line in `text`, split on `\n` and excluding the
/// newline itself (`\r` immediately before it is also excluded) — matches
/// what `str::lines()` yields, but with byte offsets into `text`.
fn line_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            let end = if i > start && text.as_bytes()[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            ranges.push(start..end);
            start = i + 1;
        }
    }
    if start <= text.len() {
        ranges.push(start..text.len());
    }
    ranges
}

/// Two lines are "loosely" equal when their whitespace-split tokens match
/// exactly — tolerant of reindentation and reflowed spacing (leading,
/// trailing, or internal run-length differences), never of an actual
/// content difference.
fn lines_match_loosely(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// Whitespace-insensitive fallback for `execute_patch`'s exact substring
/// match: every contiguous, non-overlapping byte range in `contents` whose
/// lines match `needle`'s lines via `lines_match_loosely`. Never re-flows or
/// reindents anything — only the *search* tolerates whitespace differences,
/// the eventual replacement still splices the caller's `new` text in as-is.
fn find_whitespace_insensitive(contents: &str, needle: &str) -> Vec<std::ops::Range<usize>> {
    let needle_lines: Vec<&str> = needle.lines().collect();
    if needle_lines.is_empty() {
        return Vec::new();
    }
    let content_ranges = line_ranges(contents);
    let mut matches = Vec::new();
    let mut i = 0;
    while i + needle_lines.len() <= content_ranges.len() {
        let window = &content_ranges[i..i + needle_lines.len()];
        let all_match = window
            .iter()
            .zip(&needle_lines)
            .all(|(range, needle_line)| lines_match_loosely(&contents[range.clone()], needle_line));
        if all_match {
            let start = window[0].start;
            let end = window[window.len() - 1].end;
            matches.push(start..end);
            i += needle_lines.len(); // non-overlapping
        } else {
            i += 1;
        }
    }
    matches
}

/// Third tier, advisory only: the single existing line most similar to
/// `needle`'s first non-empty line, by shared-token overlap — a hint the
/// model can use to correct `old` on retry. `None` when nothing shares even
/// one token; naming an unrelated line is worse than no hint at all.
fn suggest_closest_line(contents: &str, needle: &str) -> Option<(usize, String)> {
    let needle_first_line = needle.lines().find(|line| !line.trim().is_empty())?;
    let needle_tokens: std::collections::HashSet<&str> =
        needle_first_line.split_whitespace().collect();
    if needle_tokens.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize, &str)> = None;
    for (idx, line) in contents.lines().enumerate() {
        let line_tokens: std::collections::HashSet<&str> = line.split_whitespace().collect();
        let score = needle_tokens.intersection(&line_tokens).count();
        if score == 0 {
            continue;
        }
        if best.is_none_or(|(_, best_score, _)| score > best_score) {
            best = Some((idx, score, line));
        }
    }
    best.map(|(idx, _, line)| (idx + 1, line.trim().to_owned()))
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

/// Parse bounded `{"question", "options"}` ask-user arguments.
fn parse_ask_user_args(raw: &str) -> Result<(String, Vec<String>), ToolStepError> {
    const ALLOWED: &[&str] = &["question", "options"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if !object.keys().all(|key| ALLOWED.contains(&key.as_str()))
        || !object.contains_key("question")
        || !object.contains_key("options")
    {
        return Err(ToolStepError::Invalid);
    }
    let question = object
        .get("question")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    if question.is_empty() || question.len() > 1024 {
        return Err(ToolStepError::Invalid);
    }
    let options = object
        .get("options")
        .and_then(serde_json::Value::as_array)
        .ok_or(ToolStepError::Invalid)?;
    if options.is_empty() || options.len() > MAX_ASK_USER_OPTIONS {
        return Err(ToolStepError::Invalid);
    }
    let mut parsed = Vec::with_capacity(options.len());
    for option in options {
        let option = option.as_str().ok_or(ToolStepError::Invalid)?;
        if option.is_empty() || option.len() > 256 {
            return Err(ToolStepError::Invalid);
        }
        parsed.push(option.to_owned());
    }
    Ok((question.to_owned(), parsed))
}

/// One configured stdio MCP server (Claude `mcpServers` schema subset).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

/// Parse `mcpServers` from project settings. Names must be short
/// identifiers so the wire tool name `mcp__<server>__<tool>` stays bounded.
pub fn parse_mcp_servers(value: &serde_json::Value) -> Vec<McpServerConfig> {
    let Some(servers) = value.get("mcpServers").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let mut configs = Vec::new();
    for (name, spec) in servers {
        if configs.len() >= 8 || name.len() > 32 || name.is_empty() {
            continue;
        }
        let Some(spec) = spec.as_object() else {
            continue;
        };
        let Some(command) = spec.get("command").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let args: Vec<String> = spec
            .get("args")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        configs.push(McpServerConfig {
            name: name.clone(),
            command: command.to_owned(),
            args,
        });
    }
    configs
}

/// A live stdio MCP connection: the supervised child, its JSON-RPC session,
/// and the tools it advertised at registration.
struct McpConnection {
    server: String,
    online: bool,
    session: Option<Mutex<mcp_session_box::SessionBox>>,
}

mod mcp_session_box {
    use mcp::transport::{McpSession, StdioTransport};
    use std::process::{ChildStdin, ChildStdout};

    pub type SessionBox = McpSession<StdioTransport<ChildStdout, ChildStdin>>;
}

/// Bounded char-safe string cut.
fn truncate_str(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Generate a macOS Seatbelt profile permitting workspace writes only.
fn seatbelt_profile(workspace: &Path) -> String {
    format!(
        "(version 1)\n(deny file-write*)\n(allow file-write*\n  (subpath {workspace:?})\n  \
         (subpath \"/dev/\" )\n  (subpath \"/private/tmp/\"))\n(allow default)"
    )
}

/// Locate sandbox-exec (macOS). None = sandboxing unavailable.
fn find_sandbox_exec() -> Option<PathBuf> {
    let mut path = PathBuf::from("/usr/bin/sandbox-exec");
    if path.exists() {
        return Some(path);
    }
    path = PathBuf::from("/bin/sandbox-exec");
    if path.exists() {
        return Some(path);
    }
    None
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

/// Parse bounded `{"url", "max_bytes"?}` web-fetch arguments.
fn parse_web_fetch_args(raw: &str) -> Result<(String, usize), ToolStepError> {
    const ALLOWED: &[&str] = &["url", "max_bytes"];
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ToolStepError::Invalid)?;
    let object = value.as_object().ok_or(ToolStepError::Invalid)?;
    if !object.keys().all(|key| ALLOWED.contains(&key.as_str()))
        || !object.contains_key("url")
    {
        return Err(ToolStepError::Invalid);
    }
    let url = object
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolStepError::Invalid)?;
    if url.is_empty() || url.len() > 2048 {
        return Err(ToolStepError::Invalid);
    }
    let max_bytes = match object.get("max_bytes") {
        Some(value) => {
            let max = value.as_u64().ok_or(ToolStepError::Invalid)?;
            if max == 0 || max as usize > crate::web_fetch::MAX_FETCH_BYTES {
                return Err(ToolStepError::Invalid);
            }
            max as usize
        }
        None => crate::web_fetch::MAX_FETCH_BYTES,
    };
    Ok((url.to_owned(), max_bytes))
}

/// Parse bounded `{"prompt", "type"?, "description"?, "write_scope"?}`
/// subagent arguments. Types adopt the reference-CLI standard:
/// general-purpose | explore | plan.
fn parse_task_args(raw: &str) -> Result<TaskSpawnArgs, ToolStepError> {
    const ALLOWED: &[&str] = &["prompt", "type", "description", "write_scope"];
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
    let write_scope = match object.get("write_scope") {
        Some(value) => {
            let raw_scope = value.as_str().ok_or(ToolStepError::Invalid)?;
            let checked = checked_relative(raw_scope)?;
            Some(checked.to_string_lossy().into_owned())
        }
        None => None,
    };
    Ok(TaskSpawnArgs {
        prompt: prompt.to_owned(),
        agent_type,
        write_scope,
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
        + usize::from(object.contains_key("background"))
        + usize::from(object.contains_key("sandbox"));
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
    let sandbox = match object.get("sandbox") {
        Some(value) => value.as_bool().ok_or(ToolStepError::Invalid)?,
        None => false,
    };
    Ok(ShellArgs {
        argv: tokens,
        timeout,
        background,
        sandbox,
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

    /// Read-only trusted surface with an explicit permission lattice (subagent
    /// explore/plan scopes that must still honor the parent's deny/ask rules
    /// and persisted grants).
    pub fn read_only_with_permissions(
        root: &Path,
        permissions: PermissionLattice,
    ) -> Result<Self, ToolSetupError> {
        Ok(Self::Workspace(WorkspaceTools::open_read_only_with_permissions(
            root,
            permissions,
        )?))
    }

    /// Attach the subagent runner (no-op on the fail-closed no-op surface).
    pub fn set_subagent_runner(&mut self, runner: std::sync::Arc<dyn SubagentRunner>) {
        if let Self::Workspace(tools) = self {
            tools.set_subagent_runner(runner);
        }
    }

    /// One stderr line per tool call (no-op on the no-op surface).
    pub fn set_trace_calls(&mut self, trace: bool) {
        if let Self::Workspace(tools) = self {
            tools.set_trace_calls(trace);
        }
    }

    /// Hosts web_fetch may fetch despite resolving private (local fixtures).
    pub fn set_fetch_allowlist(&mut self, allowlist: Vec<String>) {
        if let Self::Workspace(tools) = self {
            tools.set_fetch_allowlist(allowlist);
        }
    }

    /// Attach project hook commands (pre/post tool stages).
    pub fn set_hooks(&mut self, hooks: crate::hooks::HooksConfig) {
        if let Self::Workspace(tools) = self {
            tools.set_hooks(hooks);
        }
    }

    /// Attach the shadow-diagnostics command (no-op on the no-op surface).
    pub fn set_shadow_diagnostics(
        &mut self,
        config: crate::shadow_diagnostics::ShadowDiagnosticsConfig,
    ) {
        if let Self::Workspace(tools) = self {
            tools.set_shadow_diagnostics(config);
        }
    }

    /// Register configured stdio MCP servers.
    pub fn register_mcp_servers(&mut self, servers: &[McpServerConfig]) {
        if let Self::Workspace(tools) = self {
            tools.register_mcp_servers(servers);
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
        // Every proposed name validates: a name outside the surface becomes
        // a per-call model-visible failure at execution ("unknown tool …"),
        // which the model can correct, never a turn-fatal refusal. (MCP
        // tools re-validate arguments at dispatch time.)
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
        // Each Read call gets its own key: reads target no shared resource
        // and must run concurrently with each other, not collapse onto one
        // shared `None` group and serialize. Write calls with no specific
        // target (see `write_group_key`'s fallback) get the same per-index
        // treatment; only same-path writes and same-kind shell_exec calls
        // are meant to share a key and serialize.
        let key = if tool_kind(call.tool()) == ToolKind::Read {
            Some(format!("solo:{index}"))
        } else {
            WorkspaceTools::write_group_key(call, index)
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
            handles.push((indexes.clone(), scope.spawn(move || {
                indexes
                    .into_iter()
                    .map(|index| {
                        let outcome = tools.execute_call(&calls[index], cancel);
                        (index, outcome)
                    })
                    .collect::<Vec<_>>()
            })));
        }
        for (indexes, handle) in handles {
            // A dead worker (unexpected panic) must never silently drop its
            // calls: the affected indexes surface as typed per-call failures.
            let group = match handle.join() {
                Ok(group) => group,
                Err(_) => indexes
                    .into_iter()
                    .map(|index| (index, Err(ToolStepError::Failed)))
                    .collect(),
            };
            done.push(group);
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

    fn drain_notifications(&mut self) -> Vec<ToolStepExchange> {
        let Self::Workspace(tools) = self else {
            return Vec::new();
        };
        let notices = tools.jobs.drain_notifications();
        notices
            .into_iter()
            .into_iter()
            .map(|summary| {
                let job_id = summary.split(':').next().unwrap_or("job").trim().to_owned();
                // Background job output can contain raw control bytes (e.g. a
                // literal DEL 0x7F) that `String::from_utf8_lossy` and
                // serde_json both pass through unescaped; those would fail
                // `ProposedToolCall`'s control-char validation below, so
                // sanitize before it is ever embedded in the call arguments.
                let summary = sanitize_notification_text(&summary);
                let call = ProposedToolCall::new(
                    format!("notify-{job_id}"),
                    "background_jobs",
                    format!(r#"{{"summary":{}}}"#, serde_json::json!(summary)),
                )
                .expect("fixed name/args: summary is control-byte sanitized above");
                ToolStepExchange::new(
                    vec![call],
                    vec![ToolStepResult::Succeeded {
                        call_id: format!("notify-{job_id}"),
                        summary,
                    }],
                )
            })
            .collect()
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
        let mut surface = vec![
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
                "Spawn a subagent (depth 1: it cannot spawn further agents) for a focused                  task and return its final report. Types: general-purpose (full tools),                  explore (read-only), plan (read-only). Optionally confine its writes to                  one workspace-relative path with write_scope. Arguments JSON:                  {\"prompt\":\"<task>\",\"type\":\"explore\",\"write_scope\":\"src/feature\"}.",
                arguments_schema(
                    "Spawn a subagent for a focused task",
                    serde_json::json!({
                        "prompt": {"type": "string", "description": "the subagent's task"},
                        "type": {"type": "string",
                                 "enum": ["general-purpose", "explore", "plan"],
                                 "description": "subagent type"},
                        "description": {"type": "string", "description": "short label"},
                        "write_scope": {"type": "string",
                                        "description": "workspace-relative path the subagent may write \
                                         inside, never outside (e.g. \"src/feature\")"}
                    }),
                    &["prompt"],
                ),
            ),
            ToolSurface::new(
                ASK_USER_TOOL,
                "Ask the user to choose between options. In headless runs this returns a \
                 typed refusal. Arguments JSON: {\"question\":\"...\",\"options\":[\"a\",\"b\"]}.",
                arguments_schema(
                    "Ask the user a question",
                    serde_json::json!({
                        "question": {"type": "string", "description": "the question"},
                        "options": {"type": "array", "items": {"type": "string"},
                                    "description": "2-8 answer options"}
                    }),
                    &["question", "options"],
                ),
            ),
            ToolSurface::new(
                WEB_FETCH_TOOL,
                "Fetch a web page over http/https and return readable text (HTML stripped, \
                 100 KB cap). Private/loopback hosts are refused unless allowlisted in \
                 project settings. Arguments JSON: {\"url\":\"https://example.com\"}.",
                arguments_schema(
                    "Fetch a web page",
                    serde_json::json!({
                        "url": {"type": "string", "description": "http/https URL"},
                        "max_bytes": {"type": "integer", "description": "byte cap"}
                    }),
                    &["url"],
                ),
            ),
            ToolSurface::new(
                SHELL_EXEC_TOOL,
                "Run one command inside the workspace root (argv form, no shell). Bounded \
                 output capture, wall-clock timeout, optional sandbox confinement and \
                 background execution. Arguments JSON: \
                 {\"argv\":[\"<program>\",\"<arg>\",...],\"timeout_ms\":<optional, \
                 default 60000, max 600000>,\"background\":<optional bool>,\
                 \"sandbox\":<optional bool>}.",
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
        ];

        // Dynamically registered MCP tools, bounded: first-registered-wins
        // once the cumulative size crosses MAX_MCP_TOOL_SURFACE_BYTES (see
        // its own doc comment for why this matters).
        if let Ok(registrations) = self.mcp_surface.lock() {
            let mut budget_bytes = 0usize;
            let mut omitted = 0usize;
            for (wire_name, _server, descriptor) in registrations.iter() {
                let description = descriptor
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("MCP tool {}", descriptor.name));
                let schema_bytes = serde_json::to_string(&descriptor.input_schema)
                    .map(|s| s.len())
                    .unwrap_or(0);
                let entry_bytes = wire_name.len() + description.len() + schema_bytes;
                if budget_bytes.saturating_add(entry_bytes) > MAX_MCP_TOOL_SURFACE_BYTES {
                    omitted += 1;
                    continue;
                }
                budget_bytes += entry_bytes;
                surface.push(ToolSurface::new(
                    wire_name.clone(),
                    format!("[MCP] {description}"),
                    descriptor.input_schema.clone(),
                ));
            }
            if omitted > 0 && self.trace_calls {
                crate::exec_diag::stderr_line(&format!(
                    "mcp tool surface: {omitted} tool(s) omitted past the {MAX_MCP_TOOL_SURFACE_BYTES}-byte cap"
                ));
            }
        }
        surface
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
                        cost_usd_micros: None,
                    }),
                    Ok(ModelStepOutput::Terminal {
                        text: answer.to_owned(),
                        tokens: 1,
                        cost_usd_micros: None,
                    }),
                ]),
            }
        }

        fn calls_then_answer(calls: Vec<ProposedToolCall>, answer: &str) -> Self {
            Self {
                outputs: VecDeque::from(vec![
                    Ok(ModelStepOutput::ToolCalls {
                        calls,
                        tokens: 1,
                        cost_usd_micros: None,
                    }),
                    Ok(ModelStepOutput::Terminal {
                        text: answer.to_owned(),
                        tokens: 1,
                        cost_usd_micros: None,
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
    fn write_budget_refuses_once_the_per_turn_disk_ceiling_is_reached() {
        let root = TempRoot::new("write-budget");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        // Pre-load the counter to just under the ceiling instead of
        // actually writing 64 MB — same effect, instant instead of slow.
        tools
            .bytes_written
            .store(MAX_TOTAL_WRITE_BYTES_PER_TURN - 10, Ordering::SeqCst);

        let small = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"a.txt","content":"short"}"#,
        )
        .expect("call");
        let validated = tools.validate(&small, &cancel).expect("v");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { .. } => {}
            other => panic!("a write within budget must still succeed, got {other:?}"),
        }

        let over = ProposedToolCall::new(
            "c2",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({
                "path": "b.txt",
                "content": "x".repeat(1024),
            }))
            .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&over, &cancel).expect("v");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("disk-write budget exhausted"));
            }
            other => panic!("expected a budget refusal, got {other:?}"),
        }
        // The refused write must never have touched disk.
        assert!(!root.0.join("b.txt").exists());
    }

    #[test]
    fn execute_patch_shares_the_same_per_turn_write_budget() {
        let root = TempRoot::new("patch-budget");
        fs::write(root.0.join("code.rs"), "fn a() {}\n").expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        tools
            .bytes_written
            .store(MAX_TOTAL_WRITE_BYTES_PER_TURN - 5, Ordering::SeqCst);

        let call = make_call(
            "c1",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"fn a() {}","new":"fn b() {}"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("v");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("disk-write budget exhausted"));
            }
            other => panic!("expected a budget refusal, got {other:?}"),
        }
        // The refused patch must never have touched the file.
        let contents = fs::read_to_string(root.0.join("code.rs")).expect("read");
        assert_eq!(contents, "fn a() {}\n");
    }

    #[test]
    fn workspace_write_flags_a_likely_secret_but_never_blocks_the_write() {
        let root = TempRoot::new("write-secret-advisory");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let token = format!("ghp_{}", "a".repeat(36));
        let content = format!("const TOKEN: &str = \"{token}\";\n");
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({"path": "config.rs", "content": content}))
                .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("advisory: possible secret"), "{summary}");
                assert!(summary.contains("secrets.github_token"), "{summary}");
            }
            other => panic!("expected success (advisory only), got {other:?}"),
        }
        // The write itself is never blocked or altered by the scan.
        let written = fs::read(root.0.join("config.rs")).expect("file exists");
        assert_eq!(written, content.as_bytes());

        // Ordinary content carries no advisory note at all.
        let clean_call = ProposedToolCall::new(
            "c2",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"plain.rs","content":"fn main() {}"}"#,
        )
        .expect("call");
        let validated = tools.validate(&clean_call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(!summary.contains("advisory"), "{summary}");
            }
            other => panic!("expected clean success, got {other:?}"),
        }
    }

    #[test]
    fn dismissed_secret_findings_never_resurface_after_a_rerun() {
        let root = TempRoot::new("write-secret-dismissed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let token = format!("ghp_{}", "b".repeat(36));
        let content = format!("const TOKEN: &str = \"{token}\";\n");
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({"path": "config.rs", "content": &content}))
                .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let fingerprint = match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("advisory: possible secret"), "{summary}");
                // Extract the fingerprint hex from "rule_id (fingerprint)".
                let start = summary.find('(').expect("fingerprint present") + 1;
                let end = summary[start..].find(')').expect("closing paren") + start;
                summary[start..end].to_owned()
            }
            other => panic!("expected success (advisory only), got {other:?}"),
        };

        // `tools.root()` (canonicalized, e.g. resolving macOS's /tmp ->
        // /private/tmp symlink) is what the scan itself reads/writes
        // against — not the raw `TempRoot` path, which can differ.
        let canonical_root = tools.root().to_path_buf();
        let mut store = crate::findings_store::FindingsStore::load(&canonical_root);
        store.dismiss(&fingerprint, "test fixture, not a real secret");
        store.save(&canonical_root).expect("save dismissal");

        // Same content, rewritten (e.g. the model re-saves the file):
        // the already-dismissed finding must not resurface.
        let call2 = ProposedToolCall::new(
            "c2",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({"path": "config.rs", "content": &content}))
                .expect("encode call"),
        )
        .expect("call");
        let validated2 = tools.validate(&call2, &cancel).expect("validate");
        match tools.execute(&validated2, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(!summary.contains("advisory"), "{summary}");
            }
            other => panic!("expected clean success after dismissal, got {other:?}"),
        }
    }

    #[test]
    fn workspace_write_flags_a_patch_policy_issue_but_never_blocks_the_write() {
        let root = TempRoot::new("write-patch-advisory");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let content = "name: release\npermissions: write-all\njobs: {}\n";
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({
                "path": ".github/workflows/release.yml",
                "content": content
            }))
            .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(
                    summary.contains("advisory: possible patch-policy issue"),
                    "{summary}"
                );
                assert!(summary.contains("patch.ci_permissions_broaden"), "{summary}");
            }
            other => panic!("expected success (advisory only), got {other:?}"),
        }
        // The write itself is never blocked or altered by the scan.
        let written = fs::read(root.0.join(".github/workflows/release.yml")).expect("file exists");
        assert_eq!(written, content.as_bytes());

        // Ordinary content carries no advisory note at all.
        let clean_call = ProposedToolCall::new(
            "c2",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"plain.rs","content":"fn main() {}"}"#,
        )
        .expect("call");
        let validated = tools.validate(&clean_call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(!summary.contains("advisory"), "{summary}");
            }
            other => panic!("expected clean success, got {other:?}"),
        }
    }

    #[test]
    fn writing_a_likely_secret_to_team_memory_is_blocked_not_advisory() {
        let root = TempRoot::new("write-team-memory-block");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let token = format!("ghp_{}", "c".repeat(36));
        let content = format!("- API token: {token}\n");
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({
                "path": ".rapidlm/MEMORY.md",
                "content": &content
            }))
            .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let fingerprint = match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                let detail = detail.expect("detail");
                assert!(detail.contains("write blocked"), "{detail}");
                assert!(detail.contains("mandatory"), "{detail}");
                let start = detail.find('(').expect("fingerprint present") + 1;
                let end = detail[start..].find(')').expect("closing paren") + start;
                detail[start..end].to_owned()
            }
            other => panic!("expected the write to be blocked, got {other:?}"),
        };
        // Never written: a blocked write must leave no trace.
        assert!(!root.0.join(".rapidlm/MEMORY.md").exists());

        // An ordinary file with the same content is advisory-only, never blocked.
        let ordinary = ProposedToolCall::new(
            "c2",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({"path": "notes.md", "content": &content}))
                .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&ordinary, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("advisory: possible secret"), "{summary}");
            }
            other => panic!("expected an ordinary write to succeed, got {other:?}"),
        }

        // Dismissing the fingerprint unblocks the team-memory write too.
        let canonical_root = tools.root().to_path_buf();
        let mut store = crate::findings_store::FindingsStore::load(&canonical_root);
        store.dismiss(&fingerprint, "test fixture, not a real secret");
        store.save(&canonical_root).expect("save dismissal");
        let retry = ProposedToolCall::new(
            "c3",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({
                "path": ".rapidlm/MEMORY.md",
                "content": &content
            }))
            .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&retry, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { .. } => {}
            other => panic!("expected the write to succeed after dismissal, got {other:?}"),
        }
        assert!(root.0.join(".rapidlm/MEMORY.md").exists());
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

    fn git_init(dir: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-b", "main"]);
        fs::write(dir.join("seed.txt"), b"seed\n").expect("seed");
        run(&["add", "seed.txt"]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t.invalid", "commit", "-m", "seed"]);
    }

    #[test]
    fn shadow_diagnostics_blocks_a_failing_write_and_never_touches_the_real_tree() {
        let root = TempRoot::new("shadow-fail");
        git_init(&root.0);
        let mut tools = permissive_workspace(&root.0);
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&serde_json::json!({
                "shadow_diagnostics": { "command": ["grep", "-q", "MARKER", "{path}"], "globs": ["*.txt"] }
            }))
            .expect("parsed"),
        );
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"broken.txt","content":"no marker here"}"#,
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("execute");
        match result {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("shadow diagnostics failed"));
            }
            other => panic!("expected a handled shadow-diagnostics failure, got {other:?}"),
        }
        assert!(
            !root.0.join("broken.txt").exists(),
            "a write that fails shadow diagnostics must never reach the real tree"
        );
    }

    #[test]
    fn shadow_diagnostics_applies_a_passing_write_for_real() {
        let root = TempRoot::new("shadow-pass");
        git_init(&root.0);
        let mut tools = permissive_workspace(&root.0);
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&serde_json::json!({
                "shadow_diagnostics": { "command": ["grep", "-q", "MARKER", "{path}"], "globs": ["*.txt"] }
            }))
            .expect("parsed"),
        );
        let cancel = CancellationToken::new();
        let token = format!("ghp_{}", "e".repeat(36));
        let content = format!("has MARKER inside; token {token}");
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            &serde_json::to_string(&serde_json::json!({"path": "good.txt", "content": &content}))
                .expect("encode call"),
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("execute");
        match result {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("shadow diagnostics: ok"), "{summary}");
                // The shadow-diagnostics success path scans its written
                // content too, not just the plain (no-shadow-config) path.
                assert!(summary.contains("advisory: possible secret"), "{summary}");
            }
            other => panic!("expected success, got {other:?}"),
        }
        assert_eq!(fs::read(root.0.join("good.txt")).expect("written"), content.as_bytes());
    }

    #[test]
    fn shadow_diagnostics_glob_scoping_leaves_non_matching_writes_unaffected() {
        let root = TempRoot::new("shadow-scope");
        git_init(&root.0);
        let mut tools = permissive_workspace(&root.0);
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&serde_json::json!({
                "shadow_diagnostics": { "command": ["false"], "globs": ["*.py"] }
            }))
            .expect("parsed"),
        );
        let cancel = CancellationToken::new();
        // "false" always fails, but the glob only covers *.py — this write
        // to a .txt file must bypass shadow diagnostics entirely.
        let call = ProposedToolCall::new(
            "c1",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"unrelated.txt","content":"fine"}"#,
        )
        .expect("call");
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("execute");
        assert!(matches!(result, ToolStepResult::Succeeded { .. }));
        assert_eq!(fs::read(root.0.join("unrelated.txt")).expect("written"), b"fine");
    }

    #[test]
    fn symlinked_leaf_escape_is_refused_on_read_and_write() {
        // `resolve_in_root` canonicalizes the parent directory, but a
        // symlinked *leaf* (the file argument itself) must be checked too:
        // `ln -s /etc/passwd leak.txt` then `workspace_read`/`workspace_write`
        // on `leak.txt` must not follow the link outside the workspace root.
        let root = TempRoot::new("symlink-leaf");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        let outside = TempRoot::new("symlink-leaf-outside");
        let secret = outside.0.join("secret.txt");
        fs::write(&secret, "outside-secret").expect("seed outside file");
        std::os::unix::fs::symlink(&secret, root.0.join("leak.txt")).expect("symlink");

        let read = ProposedToolCall::new("c1", WORKSPACE_READ_TOOL, r#"{"path":"leak.txt"}"#)
            .expect("call");
        let validated = tools.validate(&read, &cancel).expect("validate");
        match tools.execute(&validated, &cancel) {
            Ok(ToolStepResult::Failed { .. }) | Ok(ToolStepResult::Denied { .. }) | Err(_) => {}
            other => panic!("expected the symlinked leaf read to be refused, got {other:?}"),
        }

        let write = ProposedToolCall::new(
            "c2",
            WORKSPACE_WRITE_TOOL,
            r#"{"path":"leak.txt","content":"pwned"}"#,
        )
        .expect("call");
        let validated = tools.validate(&write, &cancel).expect("validate");
        match tools.execute(&validated, &cancel) {
            Ok(ToolStepResult::Failed { .. }) | Ok(ToolStepResult::Denied { .. }) | Err(_) => {}
            other => panic!("expected the symlinked leaf write to be refused, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(&secret).expect("outside file still readable"),
            "outside-secret",
            "the write must never follow the symlink outside the workspace root"
        );
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
    fn unknown_tools_become_model_correctable_failures() {
        let root = TempRoot::new("unknown");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = ProposedToolCall::new("c1", "mcp.call", "{}").expect("call");
        // A name outside the surface validates (it is not structural), then
        // executes as a handled failure the model can correct.
        let validated = tools.validate(&call, &cancel).expect("validate");
        let result = tools.execute(&validated, &cancel).expect("handled");
        match result {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                let detail = detail.unwrap();
                assert!(detail.contains("unknown tool `mcp.call`"));
                // A model that has already hallucinated one name is the
                // model most likely to do it again; the real tool names
                // must be spelled out right here, not just "see the tool
                // surface", so the next attempt has something concrete to
                // correct against.
                assert!(
                    detail.contains(WORKSPACE_WRITE_TOOL) && detail.contains(WORKSPACE_READ_TOOL),
                    "detail must name real tools: {detail}"
                );
            }
            other => panic!("expected handled failure, got {other:?}"),
        }
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
    fn workspace_patch_scans_the_resulting_content_like_workspace_write_does() {
        let root = TempRoot::new("patch-scanners");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        // Exact-match tier: a patch that introduces a likely secret is
        // flagged the same way workspace_write's plain path already is.
        fs::write(root.0.join("config.rs"), "const TOKEN: &str = \"placeholder\";\n").expect("seed");
        let token = format!("ghp_{}", "d".repeat(36));
        let call = make_call(
            "c1",
            WORKSPACE_PATCH_TOOL,
            &serde_json::to_string(&serde_json::json!({
                "path": "config.rs",
                "old": "\"placeholder\"",
                "new": format!("\"{token}\"")
            }))
            .expect("encode call"),
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("advisory: possible secret"), "{summary}");
            }
            other => panic!("expected patch success, got {other:?}"),
        }

        // Whitespace-insensitive tier: same scan still runs when the exact
        // substring match falls through to the loose one.
        fs::create_dir_all(root.0.join(".github/workflows")).expect("mkdir");
        fs::write(
            root.0.join(".github/workflows/release.yml"),
            "name: release\npermissions:    read-all\njobs: {}\n",
        )
        .expect("seed workflow");
        let call = make_call(
            "c2",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":".github/workflows/release.yml","old":"permissions: read-all","new":"permissions: write-all"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("whitespace-insensitive match"), "{summary}");
                assert!(
                    summary.contains("advisory: possible patch-policy issue"),
                    "{summary}"
                );
                assert!(summary.contains("patch.ci_permissions_broaden"), "{summary}");
            }
            other => panic!("expected patch success, got {other:?}"),
        }
    }

    #[test]
    fn workspace_patch_falls_back_to_whitespace_insensitive_match_when_exact_fails() {
        let root = TempRoot::new("patch-loose");
        // Real file is 4-space indented; the model's `old` guesses tabs and
        // trailing whitespace on the second line — same tokens, different
        // whitespace, so the exact substring match must fail first.
        fs::write(
            &root.0.join("code.rs"),
            "fn f() {\n    let x = 1;\n    let y = 2;\n}\n",
        )
        .expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let old = "let x = 1;\n\tlet y = 2;   ";
        let new = "let x = 10;\nlet y = 20;"; // deliberately un-indented
        let call = make_call(
            "c1",
            WORKSPACE_PATCH_TOOL,
            &format!(
                r#"{{"path":"code.rs","old":{},"new":{}}}"#,
                serde_json::to_string(old).expect("encode old"),
                serde_json::to_string(new).expect("encode new"),
            ),
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("whitespace-insensitive match"), "{summary}");
            }
            other => panic!("expected a whitespace-insensitive success, got {other:?}"),
        }
        let contents = fs::read_to_string(root.0.join("code.rs")).expect("read");
        // The replacement is spliced in exactly as given — no attempt to
        // reindent it to match the matched region's real (4-space) source
        // indentation, proven by the un-indented `new` text surviving as-is.
        assert!(contents.contains("let x = 10;\nlet y = 20;"), "{contents}");
    }

    #[test]
    fn workspace_patch_reports_ambiguity_for_multiple_whitespace_insensitive_matches() {
        let root = TempRoot::new("patch-loose-ambiguous");
        fs::write(
            &root.0.join("code.rs"),
            "fn a() {\n    let x = 1;\n}\nfn b() {\n\tlet x = 1;\n}\n",
        )
        .expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        // Exact match finds neither occurrence: `old`'s "2 spaces + tab"
        // indentation is a substring of neither the real 4-space nor the
        // real tab-indented line; the loose match finds both.
        let call = make_call(
            "c1",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"  \tlet x = 1;","new":"let x = 9;"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                let detail = detail.unwrap();
                assert!(detail.contains("2 locations"), "{detail}");
                assert!(detail.contains("ignoring whitespace"), "{detail}");
            }
            other => panic!("expected an ambiguity refusal, got {other:?}"),
        }
        // replace_all applies both.
        let call = make_call(
            "c2",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"  \tlet x = 1;","new":"let x = 9;","replace_all":true}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("replaced 2 occurrence"), "{summary}");
            }
            other => panic!("expected replace_all success, got {other:?}"),
        }
        let contents = fs::read_to_string(root.0.join("code.rs")).expect("read");
        assert_eq!(contents.matches("let x = 9;").count(), 2, "{contents}");
    }

    #[test]
    fn workspace_patch_reports_the_closest_line_when_nothing_matches_even_loosely() {
        let root = TempRoot::new("patch-hint");
        fs::write(&root.0.join("code.rs"), "fn greet(name: &str) {\n    println!(\"hi\");\n}\n")
            .expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = make_call(
            "c1",
            WORKSPACE_PATCH_TOOL,
            r#"{"path":"code.rs","old":"fn greet(name: &str, loud: bool) {","new":"x"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                let detail = detail.unwrap();
                assert!(detail.contains("not found"), "{detail}");
                assert!(
                    detail.contains("closest existing line 1: fn greet(name: &str) {"),
                    "{detail}"
                );
            }
            other => panic!("expected a not-found failure with a hint, got {other:?}"),
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

    #[cfg(unix)]
    #[test]
    fn shell_exec_flags_a_dangerous_command_but_never_blocks_it() {
        let root = TempRoot::new("shell-command-advisory");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        // A real, resolvable rm -rf against a harmless nonexistent path:
        // proves the scan runs (and the command still executes) without
        // needing to actually destroy anything.
        let call = make_call(
            "c1",
            SHELL_EXEC_TOOL,
            r#"{"argv":["rm","-rf","not-a-real-path-xyz"]}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.starts_with("exit 0"), "{summary}");
                assert!(
                    summary.contains("advisory: possible dangerous command"),
                    "{summary}"
                );
                assert!(summary.contains("command.rm_destructive"), "{summary}");
            }
            other => panic!("expected the command to still run, got {other:?}"),
        }

        // Ordinary commands carry no advisory note at all.
        let clean = make_call("c2", SHELL_EXEC_TOOL, r#"{"argv":["true"]}"#);
        let validated = tools.validate(&clean, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(!summary.contains("advisory"), "{summary}");
            }
            other => panic!("expected clean success, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_exec_flags_a_dangerous_command_on_the_background_and_sandboxed_paths_too() {
        let root = TempRoot::new("shell-command-advisory-bg-sandbox");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();

        let background = make_call(
            "c1",
            SHELL_EXEC_TOOL,
            r#"{"argv":["rm","-rf","not-a-real-path-xyz"],"background":true}"#,
        );
        let validated = tools.validate(&background, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.starts_with("started background job"), "{summary}");
                assert!(
                    summary.contains("advisory: possible dangerous command"),
                    "{summary}"
                );
                assert!(summary.contains("command.rm_destructive"), "{summary}");
            }
            other => panic!("expected background start, got {other:?}"),
        }

        let sandboxed = make_call(
            "c2",
            SHELL_EXEC_TOOL,
            r#"{"argv":["rm","-rf","not-a-real-path-xyz"],"sandbox":true}"#,
        );
        let validated = tools.validate(&sandboxed, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(
                    summary.starts_with("started sandboxed job")
                        || summary.starts_with("sandboxed "),
                    "expected a sandboxed-path summary (job-based on macOS, synchronous \
                     elsewhere): {summary}"
                );
                assert!(
                    summary.contains("advisory: possible dangerous command"),
                    "{summary}"
                );
                assert!(summary.contains("command.rm_destructive"), "{summary}");
            }
            other => panic!("expected sandboxed start, got {other:?}"),
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
    fn web_fetch_deny_rule_matches_on_the_urls_domain() {
        // `rule_subject` must extract a `domain:<host>` subject for
        // `web_fetch` calls (previously it fell through to `_ => None`, so a
        // `web_fetch(domain:...)` deny rule could never match and the call
        // fell through to the read-only auto-allow). This also proves rules
        // are checked before that auto-allow: without the fix this call
        // would attempt a real (loopback-refused) fetch instead of being
        // denied outright.
        let root = TempRoot::new("web-fetch-deny");
        let lattice = PermissionLattice::new(crate::permissions::PermissionMode::BypassPermissions)
            .with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("web_fetch(domain:evil.example*)").expect("rule"),
            }]);
        let mut tools =
            ExecTools::workspace_with_permissions(&root.0, lattice).expect("tools");
        let calls = vec![make_call(
            "c1",
            WEB_FETCH_TOOL,
            r#"{"url":"https://evil.example.com/x"}"#,
        )];
        let results = run_batch(&mut tools, &calls);
        assert!(
            matches!(
                &results[0],
                Ok(ToolStepResult::Denied { detail, .. })
                    if detail.as_deref().unwrap_or("").contains("deny rule")
            ),
            "expected the domain deny rule to block the fetch, got {:?}",
            results[0]
        );

        // A non-matching domain is unaffected by the rule (still denied only
        // by whatever the mode/allowlist would otherwise decide, not by this
        // rule): the same lattice on a different host is not caught by the
        // deny pattern.
        let calls = vec![make_call(
            "c2",
            WEB_FETCH_TOOL,
            r#"{"url":"https://fine.example.com/x"}"#,
        )];
        let results = run_batch(&mut tools, &calls);
        assert!(
            !matches!(
                &results[0],
                Ok(ToolStepResult::Denied { detail, .. })
                    if detail.as_deref().unwrap_or("").contains("deny rule")
            ),
            "the deny rule must not match an unrelated domain, got {:?}",
            results[0]
        );
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
            fn run(&self, prompt: &str, agent_type: &str, _write_scope: Option<&str>) -> Result<SubagentReport, String> {
                self.calls
                    .lock()
                    .expect("lock")
                    .push((prompt.to_owned(), agent_type.to_owned()));
                Ok(SubagentReport {
                    summary: "child finished the task".to_owned(),
                    status: "succeeded".to_owned(),
                    tool_calls: 3,
                    tokens: 512,
                    cost_usd_micros: None,
                    stop_reason: None,
                    claims: Vec::new(),
                    blockers: Vec::new(),
                    open_questions: Vec::new(),
                    patch_summary: None,
                    artifacts: Vec::new(),
                })
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
                // The structured fields from SubagentReport reach the
                // rendered summary, not just the free-text body.
                assert!(summary.contains("status=succeeded"), "{summary}");
                assert!(summary.contains("tool_calls=3"), "{summary}");
                assert!(summary.contains("tokens=512"), "{summary}");
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
    fn task_spawn_forwards_write_scope_to_the_runner_and_validates_the_path() {
        use std::sync::Mutex as StdMutex;
        struct ScopeCapturingRunner {
            seen: Arc<StdMutex<Vec<Option<String>>>>,
        }
        impl crate::exec_tools::SubagentRunner for ScopeCapturingRunner {
            fn run(
                &self,
                _prompt: &str,
                _agent_type: &str,
                write_scope: Option<&str>,
            ) -> Result<SubagentReport, String> {
                self.seen.lock().expect("lock").push(write_scope.map(str::to_owned));
                Ok(SubagentReport {
                    summary: "done".to_owned(),
                    status: "succeeded".to_owned(),
                    tool_calls: 0,
                    tokens: 1,
                    cost_usd_micros: None,
                    stop_reason: None,
                    claims: Vec::new(),
                    blockers: Vec::new(),
                    open_questions: Vec::new(),
                    patch_summary: None,
                    artifacts: Vec::new(),
                })
            }
        }

        let root = TempRoot::new("spawn-scope");
        let mut tools = permissive_workspace(&root.0);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        tools.subagents =
            Some(Arc::new(ScopeCapturingRunner { seen: Arc::clone(&seen) }) as Arc<dyn SubagentRunner>);
        let cancel = CancellationToken::new();

        // With a scope: forwarded verbatim.
        let call = make_call(
            "c1",
            TASK_SPAWN_TOOL,
            r#"{"prompt":"x","type":"explore","write_scope":"src/feature"}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("v");
        tools.execute(&validated, &cancel).expect("e");

        // Without one: forwarded as None, not a default/empty string.
        let call2 = make_call("c2", TASK_SPAWN_TOOL, r#"{"prompt":"x","type":"explore"}"#);
        let validated2 = tools.validate(&call2, &cancel).expect("v");
        tools.execute(&validated2, &cancel).expect("e");

        assert_eq!(
            seen.lock().expect("lock").clone(),
            vec![Some("src/feature".to_owned()), None]
        );

        // An escaping scope is refused the same way any other tool path is:
        // a handled, model-visible failure, never a dead turn.
        let escaping = make_call(
            "c3",
            TASK_SPAWN_TOOL,
            r#"{"prompt":"x","type":"explore","write_scope":"../outside"}"#,
        );
        let validated = tools.validate(&escaping, &cancel).expect("v");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, .. } => assert!(handled),
            other => panic!("expected a handled refusal, got {other:?}"),
        }
    }

    #[test]
    fn task_spawn_refuses_once_the_per_turn_budget_is_exhausted() {
        struct CountingRunner(Arc<AtomicU64>);
        impl crate::exec_tools::SubagentRunner for CountingRunner {
            fn run(&self, _prompt: &str, _agent_type: &str, _write_scope: Option<&str>) -> Result<SubagentReport, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(SubagentReport {
                    summary: "done".to_owned(),
                    status: "succeeded".to_owned(),
                    tool_calls: 0,
                    tokens: 1,
                    cost_usd_micros: None,
                    stop_reason: None,
                    claims: Vec::new(),
                    blockers: Vec::new(),
                    open_questions: Vec::new(),
                    patch_summary: None,
                    artifacts: Vec::new(),
                })
            }
        }

        let root = TempRoot::new("spawn-budget");
        let mut tools = permissive_workspace(&root.0);
        let ran = Arc::new(AtomicU64::new(0));
        tools.subagents = Some(Arc::new(CountingRunner(Arc::clone(&ran))) as Arc<dyn SubagentRunner>);
        let cancel = CancellationToken::new();
        let call = make_call("c1", TASK_SPAWN_TOOL, r#"{"prompt":"x","type":"explore"}"#);

        for _ in 0..MAX_SUBAGENT_SPAWNS_PER_TURN {
            let validated = tools.validate(&call, &cancel).expect("v");
            match tools.execute(&validated, &cancel).expect("e") {
                ToolStepResult::Succeeded { .. } => {}
                other => panic!("expected success under budget, got {other:?}"),
            }
        }
        assert_eq!(ran.load(Ordering::SeqCst), MAX_SUBAGENT_SPAWNS_PER_TURN);

        // One more call over budget is a handled, model-visible refusal —
        // the runner is never even invoked.
        let validated = tools.validate(&call, &cancel).expect("v");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("budget exhausted"));
            }
            other => panic!("expected a budget refusal, got {other:?}"),
        }
        assert_eq!(
            ran.load(Ordering::SeqCst),
            MAX_SUBAGENT_SPAWNS_PER_TURN,
            "the runner must not run once the budget is exhausted"
        );
    }

    #[test]
    fn task_spawn_report_renders_cost_only_when_reported() {
        struct CostRunner(Option<u64>);
        impl crate::exec_tools::SubagentRunner for CostRunner {
            fn run(&self, _prompt: &str, _agent_type: &str, _write_scope: Option<&str>) -> Result<SubagentReport, String> {
                Ok(SubagentReport {
                    summary: "done".to_owned(),
                    status: "succeeded".to_owned(),
                    tool_calls: 1,
                    tokens: 10,
                    cost_usd_micros: self.0,
                    stop_reason: None,
                    claims: Vec::new(),
                    blockers: Vec::new(),
                    open_questions: Vec::new(),
                    patch_summary: None,
                    artifacts: Vec::new(),
                })
            }
        }

        // Reported cost: the field shows up in the rendered summary.
        let root = TempRoot::new("spawn-cost");
        let mut tools = permissive_workspace(&root.0);
        tools.subagents = Some(Arc::new(CostRunner(Some(42))) as Arc<dyn SubagentRunner>);
        let call = make_call("c1", TASK_SPAWN_TOOL, r#"{"prompt":"x","type":"explore"}"#);
        let validated = tools.validate(&call, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("e") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("cost_usd_micros=42"), "{summary}");
            }
            other => panic!("expected success, got {other:?}"),
        }

        // Unreported cost (None): the field is omitted entirely, never a
        // fabricated "cost_usd_micros=0" implying a real, known zero cost.
        let root2 = TempRoot::new("spawn-no-cost");
        let mut tools2 = permissive_workspace(&root2.0);
        tools2.subagents = Some(Arc::new(CostRunner(None)) as Arc<dyn SubagentRunner>);
        let call2 = make_call("c2", TASK_SPAWN_TOOL, r#"{"prompt":"x","type":"explore"}"#);
        let validated2 = tools2.validate(&call2, &CancellationToken::new()).expect("v");
        match tools2.execute(&validated2, &CancellationToken::new()).expect("e") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(!summary.contains("cost_usd_micros"), "{summary}");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn task_spawn_report_surfaces_claims_blockers_questions_and_patch_summary() {
        struct RichRunner;
        impl crate::exec_tools::SubagentRunner for RichRunner {
            fn run(&self, _prompt: &str, _agent_type: &str, _write_scope: Option<&str>) -> Result<SubagentReport, String> {
                Ok(SubagentReport {
                    summary: "done".to_owned(),
                    status: "succeeded".to_owned(),
                    tool_calls: 2,
                    tokens: 20,
                    cost_usd_micros: None,
                    stop_reason: None,
                    claims: vec!["tests pass (satisfied)".to_owned()],
                    blockers: vec!["[policy] needs human approval".to_owned()],
                    open_questions: vec!["should this also touch the docs?".to_owned()],
                    patch_summary: Some("2 file(s) changed, +10 -3".to_owned()),
                    artifacts: vec!["sha256:deadbeef (text/plain, 12B, secret)".to_owned()],
                })
            }
        }

        let root = TempRoot::new("spawn-rich");
        let mut tools = permissive_workspace(&root.0);
        tools.subagents = Some(Arc::new(RichRunner) as Arc<dyn SubagentRunner>);
        let call = make_call("c1", TASK_SPAWN_TOOL, r#"{"prompt":"x","type":"explore"}"#);
        let validated = tools.validate(&call, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("e") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("claim: tests pass (satisfied)"), "{summary}");
                assert!(
                    summary.contains("blocker: [policy] needs human approval"),
                    "{summary}"
                );
                assert!(
                    summary.contains("open question: should this also touch the docs?"),
                    "{summary}"
                );
                assert!(
                    summary.contains("patch: 2 file(s) changed, +10 -3"),
                    "{summary}"
                );
                // A Secret-class artifact ref is still named: it's a content
                // hash + size + media type, never the actual content, so
                // there's nothing to redact.
                assert!(
                    summary.contains("artifact: sha256:deadbeef (text/plain, 12B, secret)"),
                    "{summary}"
                );
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    /// One-shot loopback HTTP fixture: serves `body` then closes.
    fn spawn_http_fixture(body: &'static str) -> std::net::SocketAddr {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn workspace_read_png_returns_inline_vision_data_url() {
        // Minimal 1x1 PNG: signature + IHDR with width=1, height=1 + IDAT.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]); // 1x1
        png.extend_from_slice(&[0, 0, 0, 10]); // IDAT length
        png.extend_from_slice(b"IDAT12345678");
        let root = TempRoot::new("png-read");
        fs::write(root.0.join("pixel.png"), &png).expect("seed");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let call = make_call("c1", WORKSPACE_READ_TOOL, r#"{"path":"pixel.png"}"#);
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("PNG image 1x1"), "{summary}");
                let data_start = summary
                    .find("DATA_URL:data:image/png;base64,")
                    .expect("data url");
                let payload = &summary[data_start + "DATA_URL:data:image/png;base64,".len()..];
                assert!(!payload.trim().is_empty(), "base64 payload present");
            }
            other => panic!("expected image read, got {other:?}"),
        }
    }

    #[test]
    fn workspace_read_pdf_extracts_text_from_uncompressed_and_flate_streams() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write as _;
        let root = TempRoot::new("pdf-read");
        let content = "BT /F1 12 Tf (the launch code is BLUE-7) Tj ET";
        // Uncompressed.
        let mut uncompressed = b"%PDF-1.4\n".to_vec();
        uncompressed
            .extend_from_slice(format!("1 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes());
        uncompressed.extend_from_slice(content.as_bytes());
        uncompressed.extend_from_slice(b"\nendstream\nendobj\n2 0 obj\n<< /Type /Page >>\nendobj\n%%EOF");
        fs::write(root.0.join("plain.pdf"), &uncompressed).expect("seed");
        // FlateDecode.
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content.as_bytes()).expect("deflate");
        let compressed = encoder.finish().expect("finish");
        let mut flated = b"%PDF-1.4\n".to_vec();
        flated.extend_from_slice(
            format!("1 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n", compressed.len())
                .as_bytes(),
        );
        flated.extend_from_slice(&compressed);
        flated.extend_from_slice(b"\nendstream\nendobj\n2 0 obj\n<< /Type /Page >>\nendobj\n%%EOF");
        fs::write(root.0.join("flat.pdf"), &flated).expect("seed");

        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        for name in ["plain.pdf", "flat.pdf"] {
            let call = make_call("c1", WORKSPACE_READ_TOOL, &format!(r#"{{"path":"{name}"}}"#));
            let validated = tools.validate(&call, &cancel).expect("validate");
            match tools.execute(&validated, &cancel).expect("execute") {
                ToolStepResult::Succeeded { summary, .. } => {
                    assert!(
                        summary.contains("BLUE-7") && summary.contains("1 page(s)"),
                        "{name}: {summary}"
                    );
                }
                other => panic!("{name}: expected extracted text, got {other:?}"),
            }
        }
    }

    #[test]
    fn web_fetch_refuses_loopback_by_default_and_fetches_when_allowlisted() {
        let root = TempRoot::new("web-fetch");
        let mut tools = permissive_workspace(&root.0);
        let cancel = CancellationToken::new();
        let addr = spawn_http_fixture("fixture body marker");
        let url = format!("http://{addr}/page");

        let call = make_call("w1", WEB_FETCH_TOOL, &format!(r#"{{"url":"{url}"}}"#));
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(
                    detail.unwrap().contains("private/loopback"),
                    "SSRF refusal must be visible to the model"
                );
            }
            other => panic!("expected SSRF refusal, got {other:?}"),
        }

        tools.set_fetch_allowlist(vec!["127.0.0.1".to_owned()]);
        let call = make_call("w2", WEB_FETCH_TOOL, &format!(r#"{{"url":"{url}"}}"#));
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("fixture body marker"), "{summary}");
            }
            other => panic!("expected fetch success, got {other:?}"),
        }
    }

    #[test]
    fn mcp_stdio_servers_register_and_dispatch_through_the_session() {
        const SERVER_SCRIPT: &str = r#"#!/usr/bin/env python3
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "demo", "version": "1.0"}}})
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": rid, "result": {"tools": [
            {"name": "echo", "description": "echoes the message",
             "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        message = req.get("params", {}).get("arguments", {}).get("message", "")
        send({"jsonrpc": "2.0", "id": rid, "result": {"content": [
            {"type": "text", "text": "echo: " + message}]}})
"#;
        let root = TempRoot::new("mcp");
        let script_path = root.0.join("mcp-echo-server.py");
        fs::write(&script_path, SERVER_SCRIPT).expect("write server");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let servers = vec![McpServerConfig {
            name: "demo".to_owned(),
            command: "python3".to_owned(),
            args: vec![script_path.display().to_string()],
        }];

        // Registration: surface gains mcp__demo__echo.
        let mut tools = permissive_workspace(&root.0);
        tools.register_mcp_servers(&servers);
        let surface: Vec<String> = tools
            .tool_surface()
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert!(
            surface.iter().any(|name| name == "mcp__demo__echo"),
            "echo tool advertised after registration: {surface:?}"
        );

        // Dead server: registration records an offline marker tool whose
        // invocation fails with a typed handled error.
        let dead = vec![McpServerConfig {
            name: "dead".to_owned(),
            command: "/nonexistent/mcp-binary".to_owned(),
            args: vec![],
        }];
        tools.register_mcp_servers(&dead);
        let surface: Vec<String> = tools
            .tool_surface()
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert!(
            surface.iter().any(|name| name == "mcp__dead__offline"),
            "offline marker tool advertised"
        );
        let call = make_call("m1", "mcp__dead__offline", "{}");
        let validated = tools.validate(&call, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("e") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("failed to start"));
            }
            other => panic!("expected offline failure, got {other:?}"),
        }

        // Dispatch through the JSON-RPC session.
        let call = make_call(
            "m2",
            "mcp__demo__echo",
            r#"{"message":"ping"}"#,
        );
        let validated = tools.validate(&call, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("dispatch") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("echo: ping"), "{summary}");
            }
            other => panic!("expected MCP success, got {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_surface_is_bounded_and_first_registered_wins() {
        let root = TempRoot::new("mcp-surface-cap");
        let tools = permissive_workspace(&root.0);
        // Each registration's schema alone is ~1 KB; enough entries blow
        // past MAX_MCP_TOOL_SURFACE_BYTES (20 KB) well before running out.
        let big_description = "x".repeat(1024);
        {
            let mut registrations = tools.mcp_surface.lock().expect("mcp surface");
            for i in 0..40 {
                registrations.push((
                    format!("mcp__srv__tool{i}"),
                    "srv".to_owned(),
                    mcp::transport::McpToolDescriptor {
                        name: format!("tool{i}"),
                        description: Some(big_description.clone()),
                        input_schema: serde_json::json!({}),
                    },
                ));
            }
        }
        let surface = tools.tool_surface();
        let mcp_tools = surface
            .iter()
            .filter(|t| t.name().starts_with("mcp__srv__"))
            .count();
        assert!(
            mcp_tools < 40,
            "the byte cap must omit some of the 40 registered tools: got {mcp_tools}"
        );
        assert!(mcp_tools > 0, "at least the earliest registrations must survive");
        // First-registered-wins: tool0 always makes it in under the cap.
        assert!(
            surface.iter().any(|t| t.name() == "mcp__srv__tool0"),
            "{surface:?}"
        );
    }

    #[test]
    fn background_job_notification_reaches_the_next_step_history() {
        // Start a quick background job; once it finishes, the driver's drain
        // produces the model notification (the turn loop replays it as a
        // synthetic exchange — see the agent-runtime turn test).
        let root = TempRoot::new("notify");
        let mut tools = permissive_workspace(&root.0);
        let start = make_call(
            "c1",
            SHELL_EXEC_TOOL,
            r#"{"argv":["sh","-c","echo all-done"],"background":true}"#,
        );
        let validated = tools.validate(&start, &CancellationToken::new()).expect("v");
        assert!(matches!(
            tools.execute(&validated, &CancellationToken::new()).expect("e"),
            ToolStepResult::Succeeded { .. }
        ));
        // Wait until the registry marks the job finished, then drain once via
        // the driver so the test controls replay timing.
        let mut notices = Vec::new();
        for _ in 0..50 {
            notices = {
                let notices = tools.jobs.drain_notifications();
                notices
            };
            if !notices.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!notices.is_empty(), "completion notice must be available");
        assert!(notices[0].contains("all-done"), "{notices:?}");
    }

    #[test]
    fn drain_notifications_survives_a_raw_control_byte_in_job_output() {
        // A background job's stdout can contain a raw DEL (0x7F) byte;
        // `String::from_utf8_lossy` and serde_json both pass it through
        // unescaped, and `ProposedToolCall::new` rejects any control byte in
        // its arguments — so building the notification call used to panic
        // via `.expect("fixed name/args")`. The driver must sanitize the
        // summary before it is ever embedded in the call arguments.
        let root = TempRoot::new("notify-control-byte");
        let mut tools =
            ExecTools::workspace_with_permissions(&root.0, PermissionLattice::new(
                crate::permissions::PermissionMode::BypassPermissions,
            ))
            .expect("tools");
        let start = make_call(
            "c1",
            SHELL_EXEC_TOOL,
            // \177 is octal for DEL (0x7F): the job prints a literal DEL
            // byte between two markers.
            r#"{"argv":["sh","-c","printf 'before\\177after'"],"background":true}"#,
        );
        let validated = tools.validate(&start, &CancellationToken::new()).expect("v");
        assert!(matches!(
            tools.execute(&validated, &CancellationToken::new()).expect("e"),
            ToolStepResult::Succeeded { .. }
        ));
        let mut exchanges = Vec::new();
        for _ in 0..50 {
            exchanges = tools.drain_notifications();
            if !exchanges.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!exchanges.is_empty(), "completion notice must be available");
        let exchange = &exchanges[0];
        assert_eq!(exchange.calls().len(), 1, "one synthetic call per job notice");
        let call = &exchange.calls()[0];
        // The built call's JSON arguments must contain no raw control bytes
        // (this is exactly the condition `ProposedToolCall::new` enforces;
        // reaching this line at all proves it did not panic).
        assert!(
            !call.arguments().chars().any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t'),
            "sanitized arguments must carry no raw control bytes: {:?}",
            call.arguments()
        );
        assert!(
            call.arguments().contains("before") && call.arguments().contains("after"),
            "surrounding text must survive sanitization: {:?}",
            call.arguments()
        );
    }

    #[test]
    fn web_fetch_caps_oversized_responses() {
        let root = TempRoot::new("web-cap");
        let mut tools = permissive_workspace(&root.0);
        tools.set_fetch_allowlist(vec!["127.0.0.1".to_owned()]);
        let cancel = CancellationToken::new();
        let big: String = "x".repeat(5000);
        let leaked: &'static str = Box::leak(big.into_boxed_str());
        let addr = spawn_http_fixture(leaked);
        let url = format!("http://{addr}/big");
        let call = make_call(
            "c1",
            WEB_FETCH_TOOL,
            &format!(r#"{{"url":"{url}","max_bytes":100}}"#),
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("execute") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(
                    summary.contains(crate::web_fetch::FETCH_TRUNCATION_MARKER),
                    "cap marker missing: {summary}"
                );
            }
            other => panic!("expected capped fetch, got {other:?}"),
        }
    }

    #[test]
    fn web_fetch_refuses_once_the_per_turn_network_budget_is_reached() {
        let root = TempRoot::new("fetch-budget");
        let mut tools = permissive_workspace(&root.0);
        tools.set_fetch_allowlist(vec!["127.0.0.1".to_owned()]);
        let cancel = CancellationToken::new();

        // Pre-load the counter to just under the ceiling — the refusal must
        // fire before any network request goes out, so no fixture server is
        // needed to prove it.
        tools
            .fetch_bytes
            .store(MAX_TOTAL_FETCH_BYTES_PER_TURN - 10, Ordering::SeqCst);

        let call = make_call(
            "c1",
            WEB_FETCH_TOOL,
            r#"{"url":"http://127.0.0.1:1/unreachable","max_bytes":1024}"#,
        );
        let validated = tools.validate(&call, &cancel).expect("validate");
        match tools.execute(&validated, &cancel).expect("handled") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                assert!(detail.unwrap().contains("web_fetch budget exhausted"));
            }
            other => panic!("expected a budget refusal, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_reads_selection_or_refuses_headless() {
        use std::sync::Mutex as StdMutex;
        let root = TempRoot::new("ask");
        let mut tools = permissive_workspace(&root.0);

        // Headless: no source wired -> typed refusal, never a turn-kill.
        let call = make_call(
            "a1",
            ASK_USER_TOOL,
            r#"{"question":"Deploy?","options":["yes","no"]}"#,
        );
        let validated = tools.validate(&call, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("e") {
            ToolStepResult::Failed { handled, detail, .. } => {
                assert!(handled);
                let detail = detail.unwrap();
                assert!(detail.contains("interactive user"), "{detail}");
            }
            other => panic!("expected headless refusal, got {other:?}"),
        }

        // Interactive: a source returns the selected option.
        let seen: Arc<StdMutex<Vec<(String, Vec<String>)>>> = Arc::default();
        let source_seen = Arc::clone(&seen);
        tools.set_ask_source(Arc::new(move |_prompt: &str, options: &[String], _budget| {
            let seen = Arc::clone(&source_seen);
            let mut guard = seen.lock().expect("lock");
            guard.push(("deploy?".to_owned(), options.to_vec()));
            Ok(options.first().cloned().unwrap_or_default())
        }));
        let call = make_call(
            "a2",
            ASK_USER_TOOL,
            r#"{"question":"Deploy now?","options":["yes","no","maybe"]}"#,
        );
        let validated = tools.validate(&call, &CancellationToken::new()).expect("v");
        match tools.execute(&validated, &CancellationToken::new()).expect("e") {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(summary.contains("user selected: yes"), "{summary}");
            }
            other => panic!("expected selection, got {other:?}"),
        }
        assert_eq!(
            seen.lock().expect("lock")[0].1,
            vec!["yes".to_owned(), "no".to_owned(), "maybe".to_owned()]
        );
    }

    #[test]
    fn tool_surface_advertises_all_sixteen_tools_with_json_schemas() {
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
                ASK_USER_TOOL,
                WEB_FETCH_TOOL,
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
