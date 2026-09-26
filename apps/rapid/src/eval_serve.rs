//! `rapid eval` — the reproducible coding benchmark (delivery goal §7).
//!
//! A task suite lives in `eval/suite/*.json`: each task seeds a scratch
//! repository with real files, states a plain-language task, carries a gold
//! patch, and names a verification command whose exit 0 is the external
//! judge. Nothing about scoring lives inside the agent.
//!
//! Two modes, reported separately and never mixed:
//!
//! - **offline** (mechanical): replays each task's gold patch through the
//!   real turn executor — the same tool dispatch, permission lattice, and
//!   ledger a live run uses — with a scripted model. This validates the
//!   harness, the tasks, and the executor end to end, deterministically. It
//!   says nothing about model quality and is labeled mechanical in every
//!   report.
//! - **live** (model quality): drives real agent CLIs (`rapid exec`, plus
//!   pinned competitor binaries when present) against identical scratch
//!   repositories with the same verification commands. Every requested arm
//!   appears in the report; arms that cannot run (binary missing, version
//!   mismatch, health-probe failure) are recorded as skipped with a TYPED
//!   infrastructure reason — never inferred from error text, never silently
//!   dropped, and no comparison is claimed without actual runs on both
//!   sides.
//!
//! Grading (version [`GRADER_VERSION`]) runs entirely outside the agent-
//! editable scratch: integrity check over protected files, the verification
//! command, then mutation checks proving the submission detects defects.
//! The command text, protected list, and mutation matrix all live in the
//! task JSON under `eval/suite/`, which agents never see.
//!
//! Output: a JSON results file (`eval/results/<mode>-<stamp>.json`) with a
//! full provenance block; exit 0 iff every task of every requested arm
//! passed. Skips are gate failures: an empty, all-skipped, or incomplete
//! comparison never exits 0.

use agent_runtime::run_turn;
use protocol::{AgentId, SessionId};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

/// Grading semantics version. Bump whenever the grading pipeline changes in
/// a way that affects which submissions pass, so retained results stay
/// interpretable alongside the code that produced them.
pub const GRADER_VERSION: &str = "2";

/// Wall-clock ceiling per live task (offline tasks finish in milliseconds).
const LIVE_TASK_TIMEOUT_SECS: u64 = 600;

/// Ceiling for the dedicated health probe (a trivial "answer READY" turn),
/// which replaces the old practice of preflighting with the first real task.
const PROBE_TIMEOUT_SECS: u64 = 120;

/// Where the suite ships.
pub const SUITE_DIR: &str = "eval/suite";

// ---------------------------------------------------------------------------
// Task schema
// ---------------------------------------------------------------------------

/// One benchmark task. Parsed manually so a task file containing fields the
/// runner does not understand is at least fully explicit about what it uses;
/// unknown extra fields are ignored rather than fatal, because the suite may
/// carry metadata for other tooling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchTask {
    /// Stable id, e.g. `bugfix-001`.
    pub id: String,
    /// `bugfix` | `multifile` | `tests` | `recovery` | `workflow`.
    pub category: String,
    /// The plain-language task a model is given.
    pub prompt: String,
    /// `(relative path, contents)` written into the scratch repo before the
    /// run — the identical starting state every agent gets.
    pub setup: Vec<(String, String)>,
    /// The gold patch: `(relative path, contents)` the offline scripted
    /// model writes. Live agents are given only the prompt.
    pub gold: Vec<(String, String)>,
    /// The external judge: a shell command run in the scratch repo root;
    /// exit 0 = the task is done correctly. The command text lives HERE,
    /// outside the agent-editable workspace — agents cannot weaken it.
    pub verify: String,
    /// A second command behavior that must FAIL on the unmodified repo (the
    /// task is not vacuous) — the anti-vacuity check every run performs.
    pub verify_fails_before: bool,
    /// Paths that must remain byte-identical to their setup contents after
    /// the run. This is how the grader detects prohibited modifications:
    /// test files, judge scripts, and harness files the agent was told not
    /// to touch. Deleting a protected path is also a violation.
    pub protected: Vec<String>,
    /// Mutation matrix for test-authoring tasks: each entry `(path,
    /// contents)` is a deliberately broken implementation the grader swaps
    /// in alone; the submission's verification command MUST FAIL against
    /// every mutant. An empty test file, zero discovered tests, weakened
    /// assertions, or hard-coded outputs survive a mutant and are rejected.
    pub mutants: Vec<(String, String)>,
}

const MAX_TASKS: usize = 64;
const MAX_FILE_BYTES: usize = 16 * 1024;

/// Load every task file in a directory, sorted by name.
pub fn load_suite(dir: &Path) -> Result<Vec<BenchTask>, String> {
    let mut tasks = Vec::new();
    for path in sorted_task_files(dir)? {
        let bytes = std::fs::read(&path).map_err(|err| err.to_string())?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|err| format!("{}: {err}", path.display()))?;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing id", path.display()))?
            .to_owned();
        let category = value
            .get("category")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing category", path.display()))?
            .to_owned();
        let prompt = value
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing prompt", path.display()))?
            .to_owned();
        let verify = value
            .get("verify")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing verify", path.display()))?
            .to_owned();
        let verify_fails_before = value
            .get("verify_fails_before")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let protected = value
            .get("protected")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .ok_or_else(|| format!("{}: non-string protected path", path.display()))
                            .map(str::to_owned)
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let mutants = value
            .get("mutants")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let file = item
                            .get("file")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| format!("{}: mutant missing file", path.display()))?;
                        let contents = item
                            .get("contents")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| {
                                format!("{}: mutant missing contents", path.display())
                            })?;
                        if contents.len() > MAX_FILE_BYTES {
                            return Err(format!(
                                "{}: mutant exceeds the byte bound",
                                path.display()
                            ));
                        }
                        Ok((file.to_owned(), contents.to_owned()))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let files = |key: &str| -> Result<Vec<(String, String)>, String> {
            let mut out = Vec::new();
            for (path, contents) in value
                .get(key)
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| format!("{}: missing {key} object", dir.display()))?
            {
                let contents = contents
                    .as_str()
                    .ok_or_else(|| format!("{field}: non-string contents", field = key))?;
                if contents.len() > MAX_FILE_BYTES {
                    return Err(format!("{key}/{path}: exceeds the byte bound"));
                }
                out.push((path.clone(), contents.to_owned()));
            }
            Ok(out)
        };
        let setup = files("setup")?;
        let gold = files("gold")?;
        if tasks.len() >= MAX_TASKS {
            return Err(format!("suite exceeds the {MAX_TASKS}-task bound"));
        }
        tasks.push(BenchTask {
            id,
            category,
            prompt,
            setup,
            gold,
            verify,
            verify_fails_before,
            protected,
            mutants,
        });
    }
    if tasks.is_empty() {
        return Err(format!("{}: no tasks found", dir.display()));
    }
    Ok(tasks)
}

fn sorted_task_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| format!("{}: {err}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    entries.sort();
    Ok(entries)
}

/// Stable digest over the suite: SHA-256 of every task file's name + bytes
/// in sorted order. Recorded in provenance so a retained result names the
/// exact suite that produced it.
pub fn suite_digest(dir: &Path) -> Result<String, String> {
    let mut hasher = Sha256::new();
    for path in sorted_task_files(dir)? {
        let bytes = std::fs::read(&path).map_err(|err| err.to_string())?;
        hasher.update(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .as_bytes(),
        );
        hasher.update([0x00]);
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Scratch repositories
// ---------------------------------------------------------------------------

/// Materialize a task's starting state in a fresh scratch repo.
pub fn materialize(root: &Path, task: &BenchTask) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|err| err.to_string())?;
    for (relative, contents) in &task.setup {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        std::fs::write(&path, contents).map_err(|err| err.to_string())?;
    }
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .output()
        .map_err(|err| err.to_string())?;
    if !git.status.success() {
        return Err("git init failed (is git installed?)".to_owned());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Supervised subprocess execution
// ---------------------------------------------------------------------------

/// Retain at most this many bytes per stream from live agents (the grok JSON
/// contract is one final JSON object; the tail is what matters) while still
/// draining every byte a verbose child produces.
const LIVE_OUTPUT_RETAIN_BYTES: usize = 512 * 1024;

/// Retain at most this many bytes per stream from verification commands —
/// enough for any test-runner failure dump, never a pipe-filling hazard.
const VERIFY_OUTPUT_RETAIN_BYTES: usize = 64 * 1024;

/// Last-resort grace for drain threads after process exit. A child that
/// exited but left a pipe held by a surviving descendant cannot EOF its
/// side; the group kill on timeout prevents that on Unix, and this grace
/// bounds it everywhere else.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// Bounded tail buffer: keeps the LAST `cap` bytes and counts everything
/// dropped in front. Retained diagnostics stay bounded no matter how
/// verbose the child, and excess bytes are still consumed (drained) so the
/// child never blocks on a full pipe.
struct BoundedTail {
    buf: VecDeque<u8>,
    cap: usize,
    dropped: u64,
    total: u64,
}

impl BoundedTail {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
            dropped: 0,
            total: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        self.buf.extend(chunk.iter().copied());
        let excess = self.buf.len().saturating_sub(self.cap);
        if excess > 0 {
            self.dropped += excess as u64;
            self.buf.drain(..excess);
        }
    }

    fn text(&self) -> String {
        let mut bytes = Vec::with_capacity(self.buf.len());
        bytes.extend(self.buf.iter().copied());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Outcome of one supervised child process.
#[derive(Clone, Debug)]
pub struct SupervisedOutput {
    /// `None` only when the child could not be waited on at all (already
    /// reaped by the platform after a group kill).
    pub success: bool,
    pub timed_out: bool,
    /// The child's exit code, when the platform reported one. `None` when
    /// killed by a signal (including the group kill on timeout).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_dropped: u64,
    pub stderr_dropped: u64,
    pub stdout_total: u64,
    pub stderr_total: u64,
}

/// Process-group discipline comes from `process-signal` — the same
/// proven primitives the supervisor, sandbox backends, and plugin-host use
/// (`isolate_process_group` = pgid == pid; `terminate_process_group` =
/// TERM, grace, unconditional group KILL, leader reaped before it
/// returns). The evaluation runner does not hand-roll its own setsid/kill.
fn supervise_spawn(command: &mut Command) -> Result<std::process::Child, std::io::Error> {
    process_signal::isolate_process_group(command);
    command.spawn()
}

fn kill_process_tree(child: &mut std::process::Child) {
    process_signal::terminate_process_group(
        child,
        process_signal::DEFAULT_TERM_GRACE,
        process_signal::DEFAULT_KILL_WAIT,
    );
}

/// Run one child to completion under supervision:
///
/// - stdout and stderr are drained CONCURRENTLY for the whole lifetime, so
///   a verbose child can never fill a pipe and block (the manufactured-
///   timeout defect); retained output is bounded, excess bytes are counted
///   and discarded.
/// - on timeout the entire process GROUP is killed and the immediate child
///   is reaped; no descendants survive.
/// - stdin, when given, is written from a separate thread and closed.
fn run_supervised(
    command: &mut Command,
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
    retain_bytes: usize,
) -> Result<SupervisedOutput, std::io::Error> {
    process_signal::isolate_process_group(command);
    command
        .stdin(if stdin_bytes.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = supervise_spawn(command)?;
    if let Some(bytes) = stdin_bytes {
        // A child that never reads stdin could fill the pipe and block the
        // write; the thread keeps the main flow free either way, and the
        // write fails once the child (or its group) is killed.
        let bytes = bytes.to_vec();
        let mut stdin = child.stdin.take().expect("stdin was piped");
        std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&bytes);
            // Dropping closes the pipe.
        });
    }
    let (stdout_tx, stdout_rx) = mpsc::channel::<BoundedTail>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<BoundedTail>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            drain_to_tail(stdout, retain_bytes, &stdout_tx);
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            drain_to_tail(stderr, retain_bytes, &stderr_tx);
        });
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let mut status: Option<std::process::ExitStatus> = None;
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    timed_out = true;
                    // TERM the group, grace, KILL the group, reap the
                    // leader — no descendants survive this call.
                    kill_process_tree(&mut child);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
    let mut stdout_tail = latest_tail(&stdout_rx);
    let mut stderr_tail = latest_tail(&stderr_rx);
    // Bounded wait for the remaining drains (pipes close when the killed
    // group's descriptors are released; the grace bounds pathological cases).
    let drain_deadline = std::time::Instant::now() + DRAIN_GRACE;
    while (stdout_tail.is_none() || stderr_tail.is_none())
        && std::time::Instant::now() < drain_deadline
    {
        if stdout_tail.is_none() {
            match stdout_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(tail) => stdout_tail = Some(tail),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stdout_tail = Some(BoundedTail::new(retain_bytes));
                }
            }
        }
        if stderr_tail.is_none() {
            match stderr_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(tail) => stderr_tail = Some(tail),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stderr_tail = Some(BoundedTail::new(retain_bytes));
                }
            }
        }
    }
    let empty = BoundedTail::new(retain_bytes);
    let stdout_tail = stdout_tail.unwrap_or(empty);
    let empty = BoundedTail::new(retain_bytes);
    let stderr_tail = stderr_tail.unwrap_or(empty);
    Ok(SupervisedOutput {
        success: !timed_out && status.is_some_and(|status| status.success()),
        timed_out,
        exit_code: status.and_then(|status| status.code()),
        stdout: stdout_tail.text(),
        stderr: stderr_tail.text(),
        stdout_dropped: stdout_tail.dropped,
        stderr_dropped: stderr_tail.dropped,
        stdout_total: stdout_tail.total,
        stderr_total: stderr_tail.total,
    })
}

fn drain_to_tail(
    mut stream: impl std::io::Read + Send + 'static,
    retain_bytes: usize,
    tx: &mpsc::Sender<BoundedTail>,
) {
    let mut tail = BoundedTail::new(retain_bytes);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => tail.push(&chunk[..n]),
            Err(_) => break,
        }
    }
    let _ = tx.send(tail);
}

/// Non-blocking sweep of a tail channel: returns the latest tail received so
/// far, if any.
fn latest_tail(rx: &mpsc::Receiver<BoundedTail>) -> Option<BoundedTail> {
    let mut latest = None;
    while let Ok(tail) = rx.try_recv() {
        latest = Some(tail);
    }
    latest
}

// ---------------------------------------------------------------------------
// Grading pipeline (grader version 2)
// ---------------------------------------------------------------------------

/// Why a submission failed grading. Infrastructure and vacuity are HARNESS
/// outcomes (skips) — they say nothing about the submission; everything
/// else is a rejection of the submission itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GradeFailure {
    /// The verification command passes on the UNMODIFIED repo: the task
    /// proves nothing, so no agent can be graded on it.
    Vacuous,
    /// The verification infrastructure itself could not run (interpreter
    /// missing, command not executable). A broken environment is not a
    /// failing submission — recorded as a skip, never as an agent failure.
    Infrastructure { detail: String },
    /// A protected file (test, judge script, harness file) was modified or
    /// deleted relative to its setup contents.
    Integrity { path: String },
    /// The verification command failed on the submission.
    Verification { output: String },
    /// A deliberately broken implementation passed the submission's tests:
    /// the tests do not detect the defect (empty tests, weakened
    /// assertions, hard-coded outputs all land here).
    MutationSurvived { file: String },
    /// The mutation run itself could not execute (environment broke while
    /// mutating) — distinct from a survived mutation.
    MutationError { file: String, detail: String },
}

impl GradeFailure {
    /// Machine-readable kind for the results JSON.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Vacuous => "vacuous",
            Self::Infrastructure { .. } => "infrastructure",
            Self::Integrity { .. } => "integrity_violation",
            Self::Verification { .. } => "verification_failed",
            Self::MutationSurvived { .. } => "mutation_survived",
            Self::MutationError { .. } => "mutation_error",
        }
    }

    /// Harness-level skips: not verdicts about the submission.
    pub fn is_harness_skip(&self) -> bool {
        matches!(self, Self::Vacuous | Self::Infrastructure { .. })
    }

    pub fn reason(&self) -> String {
        match self {
            Self::Vacuous => "vacuous task (verify passes before any change)".to_owned(),
            Self::Infrastructure { detail } => format!("infrastructure: {detail}"),
            Self::Integrity { path } => {
                format!("protected file was modified or deleted: {path}")
            }
            Self::Verification { output } => {
                format!(
                    "verification command failed after the run: {}",
                    clip(output, 400)
                )
            }
            Self::MutationSurvived { file } => format!(
                "mutation survived: tests still pass with a deliberately broken {file} — \
                 the submission does not detect the defect"
            ),
            Self::MutationError { file, detail } => {
                format!("mutation run for {file} could not execute: {detail}")
            }
        }
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    let flattened = text.replace('\n', " ");
    flattened.chars().take(max_chars).collect()
}

/// One verification-command execution with bounded output capture.
pub struct VerifyRun {
    pub passed: bool,
    pub timed_out: bool,
    pub error: Option<String>,
    /// The shell's exit code, when it exited. Codes 126/127 are sh's
    /// reserved "could not execute / not found" — a broken environment,
    /// never a verdict on the submission.
    pub exit_code: Option<i32>,
    pub output: String,
}

/// True when a verify outcome means the command itself could not run:
/// spawn/IO failure, sh's 126 (not executable) or 127 (not found). These
/// are infrastructure — distinguished from a genuine failing test.
fn verify_unrunnable(run: &VerifyRun) -> bool {
    run.error.is_some() || matches!(run.exit_code, Some(126 | 127))
}

fn verify_unrunnable_detail(run: &VerifyRun) -> String {
    if let Some(detail) = &run.error {
        format!("verification could not run: {detail}")
    } else {
        format!(
            "verification command not found or not executable (exit {})",
            run.exit_code.unwrap_or_default()
        )
    }
}

/// Run a task's verification command in `dir`. `{RAPID}` names this binary,
/// so a judge can invoke real rapid subcommands without depending on PATH.
/// Output is drained concurrently and retained up to
/// [`VERIFY_OUTPUT_RETAIN_BYTES`] per stream.
pub fn run_verify(dir: &Path, command: &str, timeout_secs: u64) -> VerifyRun {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rapid"));
    let substituted = command.replace("{RAPID}", &format!("'{}'", exe.display()));
    let mut command = Command::new("sh");
    command.arg("-c").arg(&substituted).current_dir(dir);
    let outcome = run_supervised(
        &mut command,
        None,
        Duration::from_secs(timeout_secs),
        VERIFY_OUTPUT_RETAIN_BYTES,
    );
    match outcome {
        Err(err) => VerifyRun {
            passed: false,
            timed_out: false,
            error: Some(err.to_string()),
            exit_code: None,
            output: String::new(),
        },
        Ok(out) => {
            let mut output = out.stdout.trim().to_owned();
            if !out.stderr.trim().is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("[stderr] ");
                output.push_str(out.stderr.trim());
            }
            VerifyRun {
                passed: out.success,
                timed_out: out.timed_out,
                error: None,
                exit_code: out.exit_code,
                output,
            }
        }
    }
}

/// The anti-vacuity gate, run BEFORE the agent works: the verification
/// command must FAIL on the unmodified repo, or the task proves nothing. A
/// command that cannot EXECUTE at all is a broken environment, not a vacuous
/// task — the two are distinguished here.
pub fn prechange_check(scratch: &Path, task: &BenchTask) -> Result<(), GradeFailure> {
    if !task.verify_fails_before {
        return Ok(());
    }
    let run = run_verify(scratch, &task.verify, 60);
    if verify_unrunnable(&run) {
        return Err(GradeFailure::Infrastructure {
            detail: format!("pre-change check: {}", verify_unrunnable_detail(&run)),
        });
    }
    if run.timed_out {
        return Err(GradeFailure::Infrastructure {
            detail: "pre-change check exceeded its ceiling".to_owned(),
        });
    }
    if run.passed {
        return Err(GradeFailure::Vacuous);
    }
    Ok(())
}

/// Post-run grading: integrity, verification, mutations. Every check
/// executes OUTSIDE the agent's influence — the task data (protected list,
/// command text, mutants) lives in `eval/suite/`, and this function runs in
/// the harness process, not in the scratch the agent edited.
pub fn grade_submission(scratch: &Path, task: &BenchTask) -> Result<(), GradeFailure> {
    // 1. Protected-file integrity: test files, judge scripts, and harness
    //    files must be byte-identical to what setup wrote. Deletion counts.
    for path in &task.protected {
        let Some((_, expected)) = task.setup.iter().find(|(name, _)| name == path) else {
            return Err(GradeFailure::Infrastructure {
                detail: format!("protected path {path} is not in setup"),
            });
        };
        match std::fs::read(scratch.join(path)) {
            Ok(actual) if actual.as_slice() == expected.as_bytes() => {}
            Ok(_) => return Err(GradeFailure::Integrity { path: path.clone() }),
            Err(_) => return Err(GradeFailure::Integrity { path: path.clone() }),
        }
    }
    // 2. Verification: the submission must satisfy the judge.
    let run = run_verify(scratch, &task.verify, 120);
    if verify_unrunnable(&run) {
        return Err(GradeFailure::Infrastructure {
            detail: verify_unrunnable_detail(&run),
        });
    }
    if run.timed_out {
        return Err(GradeFailure::Verification {
            output: format!(
                "verification exceeded its 120s ceiling; output: {}",
                run.output
            ),
        });
    }
    if !run.passed {
        return Err(GradeFailure::Verification { output: run.output });
    }
    // 3. Mutation checks: every deliberately broken implementation must be
    //    REJECTED by the submission. The file's pre-mutation contents are
    //    snapshotted and restored after each attempt (or the file deleted,
    //    when the submission created it), win or lose.
    for (file, contents) in &task.mutants {
        let mutant_path = scratch.join(file);
        let before = std::fs::read(&mutant_path).ok();
        if let Some(parent) = mutant_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::write(&mutant_path, contents) {
            return Err(GradeFailure::Infrastructure {
                detail: format!("could not apply mutant {file}: {err}"),
            });
        }
        let run = run_verify(scratch, &task.verify, 120);
        match before {
            Some(previous) => {
                let _ = std::fs::write(&mutant_path, previous);
            }
            None => {
                let _ = std::fs::remove_file(&mutant_path);
            }
        }
        if run.error.is_some() || run.timed_out {
            return Err(GradeFailure::MutationError {
                file: file.clone(),
                detail: run
                    .error
                    .unwrap_or_else(|| "verification timed out".to_owned()),
            });
        }
        if run.passed {
            return Err(GradeFailure::MutationSurvived { file: file.clone() });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline (mechanical) mode: the scripted gold trajectory through the real
// turn loop.
// ---------------------------------------------------------------------------

/// A model driver that performs the task's gold patch: one `workspace_write`
/// call per gold file, then a terminal answer. This exercises the real tool
/// dispatcher, permission lattice, and ledger — nothing about the outcome is
/// asserted without the grading pipeline running.
struct GoldPatchModel {
    calls: Vec<agent_runtime::ProposedToolCall>,
    step: usize,
}

impl GoldPatchModel {
    fn for_task(task: &BenchTask) -> Self {
        let calls = task
            .gold
            .iter()
            .enumerate()
            .map(|(index, (path, contents))| {
                agent_runtime::ProposedToolCall::new(
                    format!("gold-{index}"),
                    "workspace_write",
                    serde_json::json!({ "path": path, "content": contents }).to_string(),
                )
                .expect("gold call is well-formed")
            })
            .collect();
        Self { calls, step: 0 }
    }
}

impl agent_runtime::ModelDriver for GoldPatchModel {
    fn step(
        &mut self,
        input: &agent_runtime::ModelStepInput<'_>,
        _cancel: &agent_runtime::CancellationToken,
    ) -> Result<agent_runtime::ModelStepOutput, agent_runtime::ModelStepError> {
        self.step += 1;
        if self.step <= self.calls.len() {
            return Ok(agent_runtime::ModelStepOutput::ToolCalls {
                calls: vec![self.calls[self.step - 1].clone()],
                tokens: 10,
                cost_usd_micros: None,
            });
        }
        let _ = input;
        Ok(agent_runtime::ModelStepOutput::Terminal {
            text: "gold patch applied".to_owned(),
            tokens: 10,
            cost_usd_micros: None,
        })
    }
}

/// One task's outcome. `skipped` runs are recorded with the reason — a
/// report that hides its skips is a lie. Usage fields are `None` when the
/// agent cannot report them (or the task never ran); `cost_usd_micros` is
/// `None` when the provider reports no cost for this credential, which is
/// "unknown", never "free".
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TaskResult {
    pub id: String,
    pub category: String,
    pub outcome: String, // passed | failed | skipped
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Typed failure class (`vacuous`, `infrastructure`, `timeout`,
    /// `agent_non_zero`, `integrity_violation`, `verification_failed`,
    /// `mutation_survived`, `mutation_error`). Classification comes from
    /// the typed outcome, never from substring matching on error text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    pub wall_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    /// Cost ESTIMATED from the measured token split at the provider's
    /// published rates — recorded beside, never merged with, the measured
    /// `cost_usd_micros`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_estimate_usd_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<u64>,
    /// Which live agent produced this record (`rapid`, `grok`, …). `None`
    /// in offline mode, where the runner is this binary's gold trajectory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The model identity the agent's own output reported, when it does —
    /// the resolved competitor model, captured per run instead of assumed
    /// from the CLI version. `None` when the agent reports no model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Zero-based trial index in repeated-trial runs; `None` when the run
    /// was single-trial (or offline).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial: Option<u64>,
}

/// Run the suite offline: gold trajectory through the real turn loop, graded
/// only by the grading pipeline (anti-vacuity, integrity, verification,
/// mutations).
pub fn run_offline(tasks: &[BenchTask], scratch_root: &Path, trusted: bool) -> Vec<TaskResult> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut results = Vec::new();
    for task in tasks {
        let started = std::time::Instant::now();
        let scratch = scratch_root.join(format!("{}-{stamp:x}", task.id));
        let result = run_offline_task(task, &scratch, trusted);
        if std::env::var("RAPIDLM_EVAL_KEEP_SCRATCH")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            eprintln!("scratch kept: {}", scratch.display());
        } else {
            let _ = std::fs::remove_dir_all(&scratch);
        }
        results.push(TaskResult {
            id: task.id.clone(),
            category: task.category.clone(),
            outcome: match &result {
                Ok(()) => "passed".to_owned(),
                Err(failure) if failure.is_harness_skip() => "skipped".to_owned(),
                Err(_) => "failed".to_owned(),
            },
            reason: result.as_ref().err().map(|failure| failure.reason()),
            failure_kind: result.err().map(|failure| failure.kind().to_owned()),
            wall_ms: started.elapsed().as_millis(),
            tokens: Some(u64::from(task.gold.len() as u32 + 1) * 10),
            cost_usd_micros: None,
            cost_estimate_usd_micros: None,
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            num_turns: None,
            agent: None,
            model: None,
            trial: None,
        });
    }
    results
}

fn run_offline_task(task: &BenchTask, scratch: &Path, trusted: bool) -> Result<(), GradeFailure> {
    materialize(scratch, task).map_err(|detail| GradeFailure::Infrastructure { detail })?;
    // Anti-vacuity before the gold trajectory runs.
    prechange_check(scratch, task)?;
    // The gold trajectory through the REAL turn loop: tools, lattice, ledger.
    if !trusted {
        return Err(GradeFailure::Infrastructure {
            detail: "the project is not trusted; offline evaluation refuses to run with no tools"
                .to_owned(),
        });
    }
    // The gold trajectory is scripted, not a model's proposal: the calls are
    // known-good by construction, so the lattice runs in bypass mode (the
    // same seam the interactive test harness uses) and the permission gate
    // is not what is under test here — the grading pipeline is.
    let mut tools = crate::exec_tools::ExecTools::workspace_with_permissions(
        scratch,
        crate::permissions::PermissionLattice::new(
            crate::permissions::PermissionMode::BypassPermissions,
        ),
    )
    .map_err(|err| GradeFailure::Infrastructure {
        detail: err.to_string(),
    })?;
    tools.set_approval_source(std::sync::Arc::new(
        crate::approvals::LedgerApprovalSink::new(
            offline_ledger_client(scratch)
                .map_err(|detail| GradeFailure::Infrastructure { detail })?,
            SessionId::new(),
            event_ledger::event::ActorRef::new(
                event_ledger::event::ActorKind::Agent,
                &protocol::EventId::new().to_string(),
            )
            .map_err(|err| GradeFailure::Infrastructure {
                detail: err.to_string(),
            })?,
            scratch.to_path_buf(),
        ),
    ));
    let spec = agent_runtime::AgentSpec::builder(
        AgentId::new(),
        agent_runtime::AgentRole::Coder,
        task.prompt.clone(),
        protocol::WorkspaceViewId::new(),
    )
    .permissions_profile("work")
    .build()
    .map_err(|err| GradeFailure::Infrastructure {
        detail: err.to_string(),
    })?;
    let request = agent_runtime::AgentExecutionRequest::new(spec, SessionId::new());
    let mut model = GoldPatchModel::for_task(task);
    let mut events = Vec::new();
    let outcome = run_turn(
        agent_runtime::TurnSpec::new(
            protocol::TurnId::new(),
            SessionId::new(),
            AgentId::new(),
            agent_runtime::TurnBudget::new(16, Some(32), None).expect("budget"),
            &mut model,
            &mut tools,
            &mut events,
        ),
        &agent_runtime::CancellationToken::new(),
    )
    .map_err(|err| GradeFailure::Infrastructure {
        detail: err.to_string(),
    })?;
    if outcome.status() != agent_runtime::TurnStatus::Completed {
        return Err(GradeFailure::Infrastructure {
            detail: format!(
                "the gold trajectory turn did not complete: {}",
                outcome
                    .reason()
                    .map(|reason| reason.as_str())
                    .unwrap_or("unknown")
            ),
        });
    }
    let _ = request;
    grade_submission(scratch, task)
}

fn offline_ledger_client(scratch: &Path) -> Result<kernel::InProcessKernelClient, String> {
    let ledger_path = scratch.join(".rapidlm").join("sessions.db");
    if let Some(parent) = ledger_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    kernel::InProcessKernelClient::open(&ledger_path).map_err(|err| err.to_string())
}

// ---------------------------------------------------------------------------
// Live mode: real agent CLIs, identical repos, same grading pipeline.
// ---------------------------------------------------------------------------

/// Typed infrastructure failure for a live arm. Classification is structural
/// (a probe result, a missing binary, a version probe) — never inferred from
/// error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InfraError {
    BinaryMissing {
        agent: String,
    },
    VersionMismatch {
        agent: String,
        pinned: String,
        found: String,
    },
    ProbeFailed {
        agent: String,
        detail: String,
    },
    ProbeTimeout {
        agent: String,
    },
    Prep {
        agent: String,
        detail: String,
    },
}

impl InfraError {
    pub fn reason(&self) -> String {
        match self {
            Self::BinaryMissing { agent } => {
                format!("infrastructure: competitor {agent} not found on PATH")
            }
            Self::VersionMismatch {
                agent,
                pinned,
                found,
            } => format!(
                "infrastructure: competitor {agent} version mismatch (pinned {pinned}, found \
                 {found}); skipped rather than compared unpinned"
            ),
            Self::ProbeFailed { agent, detail } => {
                format!(
                    "infrastructure: {agent} health probe failed: {}",
                    clip(detail, 300)
                )
            }
            Self::ProbeTimeout { agent } => {
                format!(
                    "infrastructure: {agent} health probe exceeded its {PROBE_TIMEOUT_SECS}s ceiling"
                )
            }
            Self::Prep { agent, detail } => {
                format!("infrastructure: {agent} scratch preparation failed: {detail}")
            }
        }
    }
}

/// One pinned competitor recipe. A competitor runs only when its binary is
/// present AND reports the pinned version — an unpinned binary is skipped
/// rather than compared, and the observed version is embedded in provenance.
/// Pins recorded 2026-09-15 from the vendors' published releases and the
/// locally installed versions: grok CLI 1.0.30 (x.ai), kimi-code v0.43.0
/// (MoonshotAI, 2026-09-14; not installed locally), @qwen-code/qwen-code
/// 0.22.2 (the installed version; npm latest at pin time was 0.23.3).
/// Approval flags follow each vendor's documented non-interactive mode and
/// are re-validated by the health probe before any task runs.
pub struct AgentRecipe {
    pub name: &'static str,
    pub pinned_version: &'static str,
    pub version_flag: &'static str,
    pub prefix: &'static [&'static str],
}

pub const RECIPES: &[AgentRecipe] = &[
    AgentRecipe {
        name: "grok",
        pinned_version: "1.0.30",
        version_flag: "--version",
        // `--always-approve` is grok's auto-approval mode — the comparable
        // setting to rapid's acceptEdits — and `--output-format json`
        // returns the final answer plus provider-reported token/cost usage
        // as one JSON object: the harness's per-task usage source.
        prefix: &["--always-approve", "--output-format", "json", "-p"],
    },
    AgentRecipe {
        name: "kimi",
        // kimi-code documents a positional prompt for one-shot runs; the
        // probe below fails fast (typed) if the invocation is wrong.
        pinned_version: "0.43.0",
        version_flag: "--version",
        prefix: &[],
    },
    AgentRecipe {
        name: "qwen",
        // Qwen Code headless: `-p` print mode; auto-approval via the forked
        // gemini-cli `--yolo` switch. An unrecognized flag exits non-zero
        // immediately, which the probe records as a typed skip — it can
        // never silently hang the suite.
        pinned_version: "0.22.2",
        version_flag: "--version",
        prefix: &["--yolo", "-p"],
    },
];

/// One requested arm and whether it can run at all.
#[derive(Clone, Debug)]
pub struct ArmPlan {
    pub name: String,
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub pinned_version: String,
    pub observed_version: Option<String>,
    pub state: Result<(), InfraError>,
}

fn probe_binary(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Resolve every requested arm (ourselves + every pinned recipe) BEFORE any
/// task runs. Absent or version-mismatched competitors stay in the plan as
/// `Unavailable` so the report can include every requested arm — skipped,
/// with the typed reason, never omitted.
pub fn plan_arms(self_exe: &Path) -> Vec<ArmPlan> {
    let mut plans = vec![ArmPlan {
        name: "rapid".to_owned(),
        bin: self_exe.to_path_buf(),
        args: vec!["exec".to_owned()],
        pinned_version: env!("CARGO_PKG_VERSION").to_owned(),
        observed_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        state: Ok(()),
    }];
    for recipe in RECIPES {
        if !probe_binary(recipe.name) {
            plans.push(ArmPlan {
                name: recipe.name.to_owned(),
                bin: PathBuf::from(recipe.name),
                args: recipe.prefix.iter().map(|arg| arg.to_string()).collect(),
                pinned_version: recipe.pinned_version.to_owned(),
                observed_version: None,
                state: Err(InfraError::BinaryMissing {
                    agent: recipe.name.to_owned(),
                }),
            });
            continue;
        }
        let observed = Command::new(recipe.name)
            .arg(recipe.version_flag)
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        if !observed.contains(recipe.pinned_version) {
            plans.push(ArmPlan {
                name: recipe.name.to_owned(),
                bin: PathBuf::from(recipe.name),
                args: recipe.prefix.iter().map(|arg| arg.to_string()).collect(),
                pinned_version: recipe.pinned_version.to_owned(),
                observed_version: Some(observed.trim().to_owned()),
                state: Err(InfraError::VersionMismatch {
                    agent: recipe.name.to_owned(),
                    pinned: recipe.pinned_version.to_owned(),
                    found: observed.trim().to_owned(),
                }),
            });
            continue;
        }
        plans.push(ArmPlan {
            name: recipe.name.to_owned(),
            bin: PathBuf::from(recipe.name),
            args: recipe.prefix.iter().map(|arg| arg.to_string()).collect(),
            pinned_version: recipe.pinned_version.to_owned(),
            observed_version: Some(observed.trim().to_owned()),
            state: Ok(()),
        });
    }
    plans
}

/// Why one live task run failed. Only harness-level outcomes are skips;
/// timeouts and agent non-zero exits are FAILURES — a model that hangs or
/// crashes on a task has failed that task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFailure {
    Materialize(String),
    Spawn(String),
    Vacuous,
    PreVerifyError(String),
    AgentNonZero { snippet: String },
    Timeout,
    Grading(GradeFailure),
}

impl RunFailure {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Materialize(_) | Self::Spawn(_) | Self::PreVerifyError(_) => "infrastructure",
            Self::Vacuous => "vacuous",
            Self::AgentNonZero { .. } => "agent_non_zero",
            Self::Timeout => "timeout",
            Self::Grading(failure) => failure.kind(),
        }
    }

    pub fn is_harness_skip(&self) -> bool {
        matches!(
            self,
            Self::Vacuous | Self::Materialize(_) | Self::Spawn(_) | Self::PreVerifyError(_)
        )
    }

    pub fn reason(&self) -> String {
        match self {
            Self::Materialize(detail) | Self::Spawn(detail) | Self::PreVerifyError(detail) => {
                format!("infrastructure: {detail}")
            }
            Self::Vacuous => GradeFailure::Vacuous.reason(),
            Self::AgentNonZero { snippet } => format!("exited non-zero: {}", clip(snippet, 300)),
            Self::Timeout => format!("exceeded the {LIVE_TASK_TIMEOUT_SECS}s ceiling"),
            Self::Grading(failure) => failure.reason(),
        }
    }
}

/// Consumption an agent reported for one task. Every field is optional:
/// agents differ in what they can report, and "unknown" must stay
/// distinguishable from "zero" in the report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveUsage {
    pub tokens: Option<u64>,
    pub cost_usd_micros: Option<u64>,
    pub num_turns: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// The model identity the agent's own output reported, when it reports
    /// one — the resolved competitor model, recorded per run rather than
    /// assumed from the CLI's version string.
    pub model: Option<String>,
}

/// Published-rate catalog for the ESTIMATED cost field: `(model, (input,
/// cached-input, output) USD per million tokens, basis)`. An entry applies
/// ONLY when the resolved model identity matches exactly — pricing model A
/// at model B's rates is worse than reporting the estimate unavailable, so
/// no match means no estimate. Rates: [x.ai/api](https://x.ai/api),
/// [docs.x.ai/developers/pricing](https://docs.x.ai/developers/pricing).
const RATE_CATALOG: &[(&str, (f64, f64, f64), &str)] = &[(
    "grok-4.6",
    (2.0, 0.5, 6.0),
    "x.ai published API rates for grok-4.6 (retrieved 2026-09)",
)];

/// Resolve the estimate basis for THIS environment: the catalog entry whose
/// model matches the rapid model resolution (env override, then user
/// config) — the same resolution provenance records. `None` when the
/// active model is unconfigured, unresolvable, or not in the catalog.
fn rapid_estimate_basis() -> Option<((f64, f64, f64), &'static str)> {
    let model = match crate::user_config::select_from_process_env() {
        Ok(crate::user_config::ModelSelection::Configured { active, .. }) => {
            active.entry.wire_model().to_owned()
        }
        _ => return None,
    };
    RATE_CATALOG
        .iter()
        .find(|(catalog_model, _, _)| model.contains(catalog_model))
        .map(|(_, rates, basis)| (*rates, *basis))
}

/// Price a measured token split at the resolved basis. `None` when the
/// split itself was not reported, or when no catalog entry matches the
/// active model (the estimate is then UNAVAILABLE, never approximated).
/// When the provider did not itemize the cached subset, all input is
/// priced at the uncached rate (a stated upper bound, never silently
/// optimistic).
fn estimate_cost_micros(
    usage: &LiveUsage,
    basis: Option<((f64, f64, f64), &'static str)>,
) -> Option<u64> {
    let (rates, _) = basis?;
    let (input, output) = match (usage.input_tokens, usage.output_tokens) {
        (Some(input), Some(output)) => (input, output),
        _ => return None,
    };
    let (input_rate, cached_rate, output_rate) = rates;
    let cached = usage.cached_tokens.unwrap_or(0);
    let uncached = input.saturating_sub(cached);
    let micros =
        uncached as f64 * input_rate + cached as f64 * cached_rate + output as f64 * output_rate;
    Some(micros as u64)
}

/// Parse rapid's `exec --usage-file` output.
fn parse_rapid_usage_file(text: &str) -> Option<LiveUsage> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(LiveUsage {
        tokens: value.get("tokens").and_then(|v| v.as_u64()),
        cost_usd_micros: value.get("cost_usd_micros").and_then(|v| v.as_u64()),
        num_turns: None,
        input_tokens: value.get("input_tokens").and_then(|v| v.as_u64()),
        cached_tokens: value.get("cached_tokens").and_then(|v| v.as_u64()),
        output_tokens: value.get("output_tokens").and_then(|v| v.as_u64()),
        model: value
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    })
}

/// Parse grok CLI `--output-format json` stdout. `total_cost_usd_ticks` is
/// the provider-reported cost in 1e-10 USD units (its integer form);
/// micros are ticks / 10_000, truncated.
fn parse_grok_usage(stdout: &str) -> Option<LiveUsage> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    if value.get("type").and_then(|v| v.as_str()) == Some("error") {
        return None;
    }
    let usage = value.get("usage")?;
    Some(LiveUsage {
        tokens: usage.get("total_tokens").and_then(|v| v.as_u64()),
        cost_usd_micros: value
            .get("total_cost_usd_ticks")
            .and_then(|v| v.as_u64())
            .map(|ticks| ticks / 10_000),
        num_turns: value.get("num_turns").and_then(|v| v.as_u64()),
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        cached_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        model: value
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    })
}

/// Usage harvest after a live run: grok reports in its stdout JSON, rapid
/// in the `--usage-file` the harness passed. Called on EVERY exit path —
/// including timeouts — so partial usage is preserved when the agent
/// reported it before dying. Missing on either side stays `None` —
/// unknown, never zero.
fn collect_usage(name: &str, usage_file: Option<&Path>, stdout: &str) -> LiveUsage {
    if name == "grok" {
        parse_grok_usage(stdout).unwrap_or_default()
    } else if let Some(path) = usage_file {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|body| parse_rapid_usage_file(&body))
            .unwrap_or_default()
    } else {
        LiveUsage::default()
    }
}

/// The dedicated health probe: a trivial no-tools turn that proves the
/// model call works before any real task is attempted. Replaces the old
/// preflight that ran the first real task and classified failures by
/// substring — a real task failure could there masquerade as
/// "model not usable" and skip the whole arm.
const PROBE_PROMPT: &str = "Health probe: reply with exactly READY and finish. Do not use any tools and do not modify any files.";

/// Probe one READY arm: prepare a scratch (trust + permissions for our own
/// binary), run the trivial prompt under supervision, and classify the
/// outcome structurally (spawn error / timeout / non-zero exit).
fn probe_agent_health(plan: &ArmPlan, scratch_root: &Path, stamp: u128) -> Result<(), InfraError> {
    let dir = scratch_root.join(format!("health-{}-{stamp:x}", plan.name));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return Err(InfraError::Prep {
            agent: plan.name.clone(),
            detail: err.to_string(),
        });
    }
    let outcome = (|| {
        if plan.name == "rapid" {
            prepare_rapid_scratch(&plan.bin, &dir, false).map_err(|detail| InfraError::Prep {
                agent: plan.name.clone(),
                detail,
            })?;
        }
        let mut command = Command::new(&plan.bin);
        command.args(&plan.args).arg(PROBE_PROMPT).current_dir(&dir);
        run_supervised(
            &mut command,
            None,
            Duration::from_secs(PROBE_TIMEOUT_SECS),
            LIVE_OUTPUT_RETAIN_BYTES,
        )
        .map_err(|err| InfraError::ProbeFailed {
            agent: plan.name.clone(),
            detail: err.to_string(),
        })
    })();
    let _ = std::fs::remove_dir_all(&dir);
    match outcome {
        Ok(out) if out.timed_out => Err(InfraError::ProbeTimeout {
            agent: plan.name.clone(),
        }),
        Ok(out) if !out.success => Err(InfraError::ProbeFailed {
            agent: plan.name.clone(),
            detail: clip(&out.stderr, 300),
        }),
        Ok(_) => Ok(()),
        Err(err) => Err(err),
    }
}

/// The harness acts as the operator for its own scratch repos: grant trust
/// (each materialized repo is a fresh root) and run rapid exec in
/// acceptEdits mode — file edits auto-approved, deny rules and managed
/// ceilings still enforced. This mirrors what a real operator does
/// interactively; it grants nothing to the model that the operator did not.
/// With `grant_shell` (the equal-surface configuration) the operator also
/// pre-approves shell_exec per scratch repo, matching a competitor's
/// auto-approved shell.
fn prepare_rapid_scratch(bin: &Path, scratch: &Path, grant_shell: bool) -> Result<(), String> {
    let trust = Command::new(bin)
        .arg("trust")
        .arg("grant")
        .current_dir(scratch)
        .status()
        .map_err(|err| err.to_string())?;
    if !trust.success() {
        return Err("trust grant failed in scratch repo".to_owned());
    }
    if grant_shell {
        let shell = Command::new(bin)
            .args(["permissions", "allow", "shell_exec"])
            .current_dir(scratch)
            .status()
            .map_err(|err| err.to_string())?;
        if !shell.success() {
            return Err("shell_exec pre-approval failed in scratch repo".to_owned());
        }
    }
    Ok(())
}

/// Run the suite live for every planned arm, `trials` times per task.
/// Arms that cannot run are recorded as all-skipped with their typed
/// infrastructure reason — every requested arm appears in the report,
/// always. With `trials > 1` each attempt is recorded with its trial index
/// and `summarize` reports per-task variation; one run is never presented
/// as definitive.
pub fn run_live(
    tasks: &[BenchTask],
    scratch_root: &Path,
    arms: &[ArmPlan],
    grant_shell: bool,
    trials: u32,
) -> Vec<(String, Vec<TaskResult>)> {
    run_live_with_estimate(
        tasks,
        scratch_root,
        arms,
        grant_shell,
        trials,
        rapid_estimate_basis(),
    )
}

/// `run_live` with the estimate basis explicit — the tests' seam.
fn run_live_with_estimate(
    tasks: &[BenchTask],
    scratch_root: &Path,
    arms: &[ArmPlan],
    grant_shell: bool,
    trials: u32,
    estimate: Option<((f64, f64, f64), &'static str)>,
) -> Vec<(String, Vec<TaskResult>)> {
    let trials = trials.max(1);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Usage files live OUTSIDE the per-task scratch (which is removed after
    // each task) so a failed task's spend is still recorded.
    let usage_dir = scratch_root.join("usage");
    let _ = std::fs::create_dir_all(&usage_dir);
    let mut per_agent = Vec::new();
    for plan in arms {
        if let Err(infra) = &plan.state {
            per_agent.push((
                plan.name.clone(),
                skip_all(tasks, &plan.name, &infra.reason()),
            ));
            continue;
        }
        // Health probe BEFORE any real task: a typed infrastructure gate.
        if let Err(infra) = probe_agent_health(plan, scratch_root, stamp) {
            per_agent.push((
                plan.name.clone(),
                skip_all(tasks, &plan.name, &infra.reason()),
            ));
            continue;
        }
        let mut results = Vec::new();
        for trial in 0..trials {
            for task in tasks {
                let started = std::time::Instant::now();
                let scratch =
                    scratch_root.join(format!("{}-{}-{}-{stamp:x}", plan.name, trial, task.id));
                let usage_path = usage_dir.join(format!(
                    "{}-{}-{}-{stamp:x}.json",
                    plan.name, trial, task.id
                ));
                let usage_file = if plan.name == "rapid" {
                    Some(usage_path.as_path())
                } else {
                    None
                };
                let (result, usage) = run_live_agent(plan, task, &scratch, grant_shell, usage_file);
                let _ = std::fs::remove_dir_all(&scratch);
                let (outcome, reason, failure_kind) = match &result {
                    Ok(()) => ("passed".to_owned(), None, None),
                    Err(failure) => (
                        if failure.is_harness_skip() {
                            "skipped"
                        } else {
                            "failed"
                        }
                        .to_owned(),
                        Some(failure.reason()),
                        Some(failure.kind().to_owned()),
                    ),
                };
                results.push(TaskResult {
                    id: task.id.clone(),
                    category: task.category.clone(),
                    outcome,
                    reason,
                    failure_kind,
                    trial: (trials > 1).then_some(u64::from(trial)),
                    wall_ms: started.elapsed().as_millis(),
                    tokens: usage.tokens,
                    cost_usd_micros: usage.cost_usd_micros,
                    cost_estimate_usd_micros: estimate_cost_micros(&usage, estimate),
                    input_tokens: usage.input_tokens,
                    cached_tokens: usage.cached_tokens,
                    output_tokens: usage.output_tokens,
                    num_turns: usage.num_turns,
                    agent: Some(plan.name.clone()),
                    model: usage.model,
                });
            }
        }
        per_agent.push((plan.name.clone(), results));
    }
    per_agent
}

fn skip_all(tasks: &[BenchTask], agent: &str, reason: &str) -> Vec<TaskResult> {
    tasks
        .iter()
        .map(|task| TaskResult {
            id: task.id.clone(),
            category: task.category.clone(),
            outcome: "skipped".to_owned(),
            reason: Some(reason.to_owned()),
            failure_kind: Some("infrastructure".to_owned()),
            wall_ms: 0,
            tokens: None,
            cost_usd_micros: None,
            cost_estimate_usd_micros: None,
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            num_turns: None,
            agent: Some(agent.to_owned()),
            model: None,
            trial: None,
        })
        .collect()
}

/// Drive one live agent on one task: materialize the identical repo,
/// anti-vacuity check, run the agent's argv with the prompt appended under
/// supervision (concurrent drain, group kill on timeout, partial usage
/// preserved), then grade the submission with the full pipeline.
fn run_live_agent(
    plan: &ArmPlan,
    task: &BenchTask,
    scratch: &Path,
    grant_shell: bool,
    usage_file: Option<&Path>,
) -> (Result<(), RunFailure>, LiveUsage) {
    if let Err(detail) = materialize(scratch, task) {
        return (Err(RunFailure::Materialize(detail)), LiveUsage::default());
    }
    if let Err(failure) = prechange_check(scratch, task) {
        // The pre-change gate distinguishes a vacuous task (verify passes
        // with no change — proves nothing, a harness skip) from a broken
        // verification environment (interpreter missing, command not
        // executable, ceiling too low — typed infrastructure, never an
        // agent failure). Both are skips, but with different kinds.
        return (
            Err(match failure {
                GradeFailure::Vacuous => RunFailure::Vacuous,
                GradeFailure::Infrastructure { detail } => {
                    RunFailure::PreVerifyError(format!("pre-change check: {detail}"))
                }
                other => RunFailure::PreVerifyError(other.reason()),
            }),
            LiveUsage::default(),
        );
    }
    let mut argv = plan.args.clone();
    if let Some(path) = usage_file {
        argv.push("--usage-file".to_owned());
        argv.push(path.display().to_string());
    }
    argv.push(task.prompt.clone());
    let mut command = Command::new(&plan.bin);
    command.args(&argv).current_dir(scratch);
    if plan.name == "rapid" {
        if let Err(detail) = prepare_rapid_scratch(&plan.bin, scratch, grant_shell) {
            return (Err(RunFailure::Spawn(detail)), LiveUsage::default());
        }
        command.env("RAPIDLM_PERMISSION_MODE", "acceptEdits");
    }
    let outcome = match run_supervised(
        &mut command,
        None,
        Duration::from_secs(LIVE_TASK_TIMEOUT_SECS),
        LIVE_OUTPUT_RETAIN_BYTES,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            return (
                Err(RunFailure::Spawn(format!(
                    "{} could not start: {err}",
                    plan.name
                ))),
                LiveUsage::default(),
            );
        }
    };
    let mut text = outcome.stdout.clone();
    if !outcome.stderr.trim().is_empty() {
        text.push_str("\n[stderr] ");
        text.push_str(outcome.stderr.trim());
    }
    // Usage is collected on EVERY path — including timeouts and non-zero
    // exits — so partial measurements survive failed attempts.
    let usage = collect_usage(&plan.name, usage_file, &text);
    if outcome.timed_out {
        return (Err(RunFailure::Timeout), usage);
    }
    if !outcome.success {
        return (Err(RunFailure::AgentNonZero { snippet: text }), usage);
    }
    if let Err(failure) = grade_submission(scratch, task) {
        return (Err(RunFailure::Grading(failure)), usage);
    }
    (Ok(()), usage)
}

// ---------------------------------------------------------------------------
// Accounting
// ---------------------------------------------------------------------------

/// Per-agent efficiency accounting. The headline rate is TOTAL measured
/// usage across ALL attempts (passed AND failed) divided by verified
/// successes — spend includes failed work. Successful-attempt averages are
/// reported separately. Missing usage is unknown, never zero: coverage and
/// the lower-bound flag are always exposed, and a rate is only published
/// when at least one attempt was measured. Verified successes come from the
/// grading pipeline (`outcome == "passed"`), never from a turn merely
/// completing.
fn agent_usage_metrics(results: &[TaskResult], agent: &str) -> serde_json::Value {
    let attempts: Vec<&TaskResult> = results
        .iter()
        .filter(|r| r.agent.as_deref().unwrap_or("runner") == agent)
        .collect();
    let successes = attempts.iter().filter(|r| r.outcome == "passed").count();
    let successful: Vec<&&TaskResult> = attempts.iter().filter(|r| r.outcome == "passed").collect();
    let mut entry = serde_json::Map::new();
    entry.insert("attempts".to_owned(), serde_json::json!(attempts.len()));
    entry.insert(
        "verified_successes".to_owned(),
        serde_json::json!(successes),
    );
    if successes == 0 {
        entry.insert(
            "note".to_owned(),
            serde_json::json!(
                "no verified successes; per-success rates are undefined and intentionally absent"
            ),
        );
    }
    // Tokens: all measured attempts over verified successes.
    let measured: Vec<u64> = attempts.iter().filter_map(|r| r.tokens).collect();
    let missing = attempts.len() - measured.len();
    if !measured.is_empty() && successes > 0 {
        let total: u64 = measured.iter().sum();
        let successful_measured: Vec<u64> = successful.iter().filter_map(|r| r.tokens).collect();
        let mut block = serde_json::json!({
            "tokens_all_measured_attempts_per_success": total / successes as u64,
            "attempts_measured": measured.len(),
            "attempts_missing_usage": missing,
            "complete_coverage": missing == 0,
            // With missing measurements the published rate can only grow,
            // so it is a LOWER bound on true spend per success.
            "lower_bound": missing > 0,
        });
        if !successful_measured.is_empty() {
            block["successful_attempts_only_average"] = serde_json::json!(
                successful_measured.iter().sum::<u64>() / successful_measured.len() as u64
            );
            block["successful_attempts_measured"] = serde_json::json!(successful_measured.len());
        }
        entry.insert("tokens_per_verified_success".to_owned(), block);
    }
    // Measured cost: same semantics, kept strictly separate from the
    // published-rate estimate below.
    let cost_measured: Vec<u64> = attempts.iter().filter_map(|r| r.cost_usd_micros).collect();
    if !cost_measured.is_empty() && successes > 0 {
        entry.insert(
            "cost_per_verified_success".to_owned(),
            serde_json::json!({
                "usd_micros_all_measured_attempts_per_success":
                    cost_measured.iter().sum::<u64>() / successes as u64,
                "attempts_measured": cost_measured.len(),
                "attempts_missing_usage": attempts.len() - cost_measured.len(),
                "lower_bound": cost_measured.len() < attempts.len(),
            }),
        );
    }
    // Estimated cost: measured token split priced at published rates — an
    // estimate, never merged with measured charges.
    let est_measured: Vec<u64> = attempts
        .iter()
        .filter_map(|r| r.cost_estimate_usd_micros)
        .collect();
    if !est_measured.is_empty() && successes > 0 {
        entry.insert(
            "cost_estimate_per_verified_success".to_owned(),
            serde_json::json!({
                "usd_micros_all_measured_attempts_per_success":
                    est_measured.iter().sum::<u64>() / successes as u64,
                "attempts_measured": est_measured.len(),
                "basis": "measured token split priced at the provider's published rates — an estimate, not a measurement",
                "lower_bound": est_measured.len() < attempts.len(),
            }),
        );
    }
    serde_json::Value::Object(entry)
}

/// Summary counts + the JSON report body. Mechanical and live reports are
/// written to separate files and never summed together.
pub fn summarize(mode: &str, results: &[TaskResult]) -> (serde_json::Value, String) {
    let passed = results.iter().filter(|r| r.outcome == "passed").count();
    let failed = results.iter().filter(|r| r.outcome == "failed").count();
    let skipped = results.iter().filter(|r| r.outcome == "skipped").count();
    let mut agent_names: Vec<String> = Vec::new();
    for result in results {
        let name = result.agent.clone().unwrap_or_else(|| "runner".to_owned());
        if !agent_names.contains(&name) {
            agent_names.push(name);
        }
    }
    let mut agent_metrics = serde_json::Map::new();
    let mut outcome_metrics = serde_json::Map::new();
    let mut metric_lines: Vec<String> = Vec::new();
    for agent in &agent_names {
        let metrics = agent_usage_metrics(results, agent);
        if let Some(tokens) = metrics.get("tokens_per_verified_success") {
            metric_lines.push(format!(
                "  {agent} tokens/verified-success: {} (all measured attempts / {} successes; \
                 coverage {}/{}, lower_bound={})",
                tokens["tokens_all_measured_attempts_per_success"]
                    .as_u64()
                    .unwrap_or(0),
                metrics["verified_successes"].as_u64().unwrap_or(0),
                tokens["attempts_measured"].as_u64().unwrap_or(0),
                metrics["attempts"].as_u64().unwrap_or(0),
                tokens["lower_bound"].as_bool().unwrap_or(false),
            ));
        }
        if let Some(cost) = metrics.get("cost_per_verified_success") {
            metric_lines.push(format!(
                "  {agent} cost/verified-success: ${:.4} (measured, all attempts / successes)",
                cost["usd_micros_all_measured_attempts_per_success"]
                    .as_u64()
                    .unwrap_or(0) as f64
                    / 1_000_000.0
            ));
        }
        if let Some(est) = metrics.get("cost_estimate_per_verified_success") {
            metric_lines.push(format!(
                "  {agent} cost-estimate/verified-success: ${:.4} (published-rate estimate)",
                est["usd_micros_all_measured_attempts_per_success"]
                    .as_u64()
                    .unwrap_or(0) as f64
                    / 1_000_000.0
            ));
        }
        let outcomes = results
            .iter()
            .filter(|r| r.agent.as_deref().unwrap_or("runner") == agent);
        outcome_metrics.insert(
            agent.clone(),
            serde_json::json!({
                "total": outcomes.clone().count(),
                "passed": outcomes.clone().filter(|r| r.outcome == "passed").count(),
                "failed": outcomes.clone().filter(|r| r.outcome == "failed").count(),
                "skipped": outcomes.filter(|r| r.outcome == "skipped").count(),
            }),
        );
        agent_metrics.insert(agent.clone(), metrics);
    }
    // Repeated-trial variation: with trials > 1, report per-task pass
    // counts so one run is never presented as definitive. A task that
    // passed SOME trials but not all is flaky; a task that never passed is
    // listed too.
    let mut variation: Option<serde_json::Value> = None;
    let max_trial = results.iter().filter_map(|r| r.trial).max();
    if let Some(max_trial) = max_trial {
        let trials = max_trial + 1;
        if trials > 1 {
            let mut variation_by_agent = serde_json::Map::new();
            for agent in &agent_names {
                let attempts: Vec<&TaskResult> = results
                    .iter()
                    .filter(|r| r.agent.as_deref().unwrap_or("runner") == agent)
                    .filter(|r| r.outcome != "skipped")
                    .collect();
                let mut ids: Vec<&str> = attempts.iter().map(|r| r.id.as_str()).collect();
                ids.sort_unstable();
                ids.dedup();
                let mut per_task = serde_json::Map::new();
                let mut flaky = Vec::new();
                let mut never = Vec::new();
                for id in ids {
                    let task_results: Vec<&TaskResult> =
                        attempts.iter().filter(|r| r.id == id).copied().collect();
                    let passes = task_results
                        .iter()
                        .filter(|r| r.outcome == "passed")
                        .count();
                    per_task.insert(id.to_owned(), serde_json::json!(passes));
                    if passes == 0 {
                        never.push(id.to_owned());
                    } else if passes < task_results.len() {
                        flaky.push(id.to_owned());
                    }
                }
                variation_by_agent.insert(
                    agent.clone(),
                    serde_json::json!({
                        "trials": trials,
                        "per_task_passes": per_task,
                        "flaky_tasks": flaky,
                        "never_passed": never,
                    }),
                );
                if !flaky.is_empty() {
                    metric_lines.push(format!(
                        "  {agent} VARIATION across {trials} trials: flaky tasks (passed some \
                         trials, not all): {}",
                        flaky.join(", ")
                    ));
                }
            }
            variation = Some(serde_json::Value::Object(variation_by_agent));
        }
    }
    let mut report = serde_json::json!({
        "mode": mode,
        "kind": if mode == "offline" { "mechanical validation (scripted gold trajectories; says nothing about model quality)" } else { "live model quality (agents given prompts only; the verification command is the judge)" },
        "grading_version": GRADER_VERSION,
        "tasks": results.len(),
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "results": results,
    });
    if !agent_metrics.is_empty() {
        report["per_agent_usage"] = serde_json::Value::Object(agent_metrics);
    }
    if !outcome_metrics.is_empty() {
        report["per_agent_outcomes"] = serde_json::Value::Object(outcome_metrics);
    }
    if let Some(variation) = variation {
        report["variation"] = variation;
    }
    let mut lines = vec![format!(
        "{mode}: {}/{} passed, {failed} failed, {skipped} skipped",
        passed,
        results.len()
    )];
    lines.extend(metric_lines);
    for result in results {
        if result.outcome != "passed" {
            lines.push(format!(
                "  {} [{}]{} {}: {}",
                result.id,
                result.category,
                result
                    .agent
                    .as_ref()
                    .map(|agent| format!(" ({agent})"))
                    .unwrap_or_default(),
                result.outcome,
                result.reason.as_deref().unwrap_or("")
            ));
        }
    }
    (report, lines.join("\n"))
}

/// Results directory for a run.
pub fn results_dir(root: &Path) -> PathBuf {
    root.join("eval").join("results")
}

/// The gate: exit 0 only when every task of every requested arm passed.
/// Empty result sets, any failure, and any skip (missing competitor, failed
/// health probe, vacuous task) all fail the gate — an incomplete comparison
/// can never pass.
pub fn eval_exit_code(results: &[TaskResult]) -> i32 {
    if results.is_empty() {
        return 1;
    }
    if results.iter().any(|result| result.outcome != "passed") {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

/// Everything a retained result needs in order to be interpreted and
/// reproduced later: repository state, runner + grading versions, suite
/// digest, model identity, permissions, budgets, environment, and the exact
/// arm plan with observed binary versions.
#[allow(clippy::too_many_arguments)]
pub fn build_provenance(
    mode: &str,
    suite_dir: &Path,
    tasks: &[BenchTask],
    arms: &[ArmPlan],
    grant_shell: bool,
    trials: u32,
    requested_arms: Option<&[String]>,
    estimate_basis: Option<&'static str>,
) -> serde_json::Value {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|out| out.status.success() && !out.stdout.is_empty());
    let suite_hash = suite_digest(suite_dir).unwrap_or_else(|_| "unknown".to_owned());
    let model = match crate::user_config::select_from_process_env() {
        Ok(crate::user_config::ModelSelection::Configured { active, .. }) => {
            serde_json::json!(active.entry.wire_model())
        }
        Ok(crate::user_config::ModelSelection::Unconfigured { .. }) => {
            serde_json::json!("unconfigured")
        }
        Err(err) => serde_json::json!(format!("unresolvable: {err}")),
    };
    let model_env = std::env::var("RAPIDLM_MODEL").ok();
    serde_json::json!({
        "timestamp_unix_secs": timestamp,
        "mode": mode,
        "repository": {
            "commit": commit,
            "dirty": dirty,
        },
        "runner": {
            "name": "rapid",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "grading": {
            "version": GRADER_VERSION,
            "vacuity_timeout_secs": 60,
            "verify_timeout_secs": 120,
            "mutation_timeout_secs": 120,
        },
        "suite": {
            "dir": suite_dir.display().to_string(),
            "tasks": tasks.len(),
            "hash_sha256": suite_hash,
            "task_ids": tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
        },
        "model": {
            "rapid_active_model": model,
            "rapid_model_env_override": model_env,
        },
        "permissions": {
            "rapid_permission_mode": "acceptEdits",
            "grant_shell": grant_shell,
            "trust_granted_per_scratch": true,
        },
        "budgets": {
            "live_task_timeout_secs": LIVE_TASK_TIMEOUT_SECS,
            "probe_timeout_secs": PROBE_TIMEOUT_SECS,
            "max_tasks": MAX_TASKS,
            "max_file_bytes": MAX_FILE_BYTES,
            "trials": trials,
        },
        "cost_estimate": {
            "basis": estimate_basis,
            "applies_only_when_model_matches": true,
        },
        "invocation": {
            "requested_arms": requested_arms,
            "argv_by_arm": arms
                .iter()
                .map(|arm| {
                    serde_json::json!({
                        "name": arm.name,
                        "bin": arm.bin.display().to_string(),
                        // The prompt is appended after these args; the
                        // usage-file flag is appended for the rapid arm.
                        "args": arm.args,
                    })
                })
                .collect::<Vec<_>>(),
        },
        "environment": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
        },
        "arms": arms
            .iter()
            .map(|arm| serde_json::json!({
                "name": arm.name,
                "pinned_version": arm.pinned_version,
                "observed_version": arm.observed_version,
                "runnable": arm.state.is_ok(),
                "skip_reason": arm.state.as_ref().err().map(|err| err.reason()),
            }))
            .collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub const EVAL_USAGE: &str = "usage: rapid eval --offline [--suite <dir>] [--scratch <dir>] | rapid eval --live [--grant-shell] [--trials <n>] [--suite <dir>] [--scratch <dir>]

Run the reproducible coding benchmark.

Suites:
  eval/suite           representative model-quality tasks (navigate, refactor,
                       context, errors, integration) — the default
  eval/suite-smoke     40 mechanical smoke cases (harness validation; NOT a
                       model-quality evaluation)
  eval/suite-heldout   held-out variants, never used for tuning

Modes:
  --offline   mechanical validation: each task's gold patch replays through
              the real turn executor (tools, permission lattice, ledger) and
              the task's verification command is the only judge. Deterministic;
              says nothing about model quality.
  --live      model quality: real agent CLIs run on identical scratch repos
              with the same prompts and the same grading pipeline. A health
              probe runs first; arms that cannot run (binary missing, version
              mismatch, probe failure) are recorded as skipped with a typed
              reason. Every requested arm appears in the report.
  --trials <n>  (live) run every task n times and report per-task variation;
              one run is never presented as definitive.
  --arms <names>  (live) comma-separated arm subset to run (e.g. --arms
              rapid,grok). The full arm plan stays in provenance; the gate
              judges only what ran.
  --grant-shell  (live, rapid arm only) pre-approve shell_exec per scratch
              repo — the equal-tool-surface configuration, matching a
              competitor that auto-approves shell. Without it rapid runs
              acceptEdits (shell denied) and the report says so.

Grading (version 2) runs outside the agent-editable workspace: protected-file
integrity, the verification command, and mutation checks (a submission must
detect deliberately broken implementations).

Results are written to eval/results/<mode>-<stamp>.json with full provenance
(repository commit, suite hash, model, permissions, budgets, arm versions).
Exit code: 0 only if EVERY task of EVERY requested arm passed. Skips fail
the gate — an empty, all-skipped, or incomplete comparison never passes.
";

/// `rapid eval`.
pub fn run_eval(args: &[String]) -> Result<i32, crate::p9_commands::P9CommandError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{EVAL_USAGE}");
        return Ok(0);
    }
    let offline = args.iter().any(|arg| arg == "--offline");
    let live = args.iter().any(|arg| arg == "--live");
    if offline == live {
        eprint!("{EVAL_USAGE}");
        return Err(crate::p9_commands::P9CommandError::Usage);
    }
    let suite = args
        .iter()
        .position(|arg| arg == "--suite")
        .and_then(|position| args.get(position + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SUITE_DIR));
    let scratch = args
        .iter()
        .position(|arg| arg == "--scratch")
        .and_then(|position| args.get(position + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rapid-eval-scratch"));
    let _ = std::fs::create_dir_all(&scratch);
    let tasks = load_suite(&suite).map_err(|err| {
        crate::p9_commands::P9CommandError::Agent(format!("suite load failed: {err}"))
    })?;
    let Some((_root, trusted)) = crate::interactive::workflow_workspace_root() else {
        eprintln!("rapid eval: no project workspace resolved");
        return Err(crate::p9_commands::P9CommandError::Usage);
    };

    let mode = if offline { "offline" } else { "live" };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let grant_shell = args.iter().any(|arg| arg == "--grant-shell");
    let trials = args
        .iter()
        .position(|arg| arg == "--trials")
        .and_then(|position| args.get(position + 1))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|trials| *trials >= 1)
        .unwrap_or(1);
    let all_arms = plan_arms(&std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rapid")));
    // `--arms rapid,grok` restricts the RUN to the named arms. The full
    // plan stays in provenance (so the record shows what else was
    // requested of the environment); the gate only judges what ran.
    let requested_arms: Option<Vec<String>> = args
        .iter()
        .position(|arg| arg == "--arms")
        .and_then(|position| args.get(position + 1))
        .map(|value| {
            value
                .split(',')
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .filter(|names: &Vec<String>| !names.is_empty());
    let arms: Vec<ArmPlan> = match &requested_arms {
        Some(names) => {
            // A typo beside a valid name used to shrink the run silently:
            // the gate then judged a subset while looking like it judged
            // the request. Unknown names fail the invocation instead.
            let unknown: Vec<&str> = names
                .iter()
                .filter(|name| !all_arms.iter().any(|arm| &arm.name == *name))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                let known = all_arms
                    .iter()
                    .map(|arm| arm.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(crate::p9_commands::P9CommandError::Agent(format!(
                    "unknown arm(s): {} — known arms: {known}",
                    unknown.join(", ")
                )));
            }
            all_arms
                .into_iter()
                .filter(|arm| names.contains(&arm.name))
                .collect()
        }
        None => all_arms,
    };
    let provenance = build_provenance(
        mode,
        &suite,
        &tasks,
        &arms,
        grant_shell,
        trials,
        requested_arms.as_deref(),
        rapid_estimate_basis().map(|(_, basis)| basis),
    );
    let results = if offline {
        run_offline(&tasks, &scratch, trusted)
    } else {
        run_live(&tasks, &scratch, &arms, grant_shell, trials)
            .into_iter()
            .flat_map(|(_agent, results)| results)
            .collect::<Vec<_>>()
    };
    let (mut report, summary) = summarize(mode, &results);
    report["provenance"] = provenance;
    let results_dir = results_dir(&std::env::current_dir().unwrap_or_default());
    let _ = std::fs::create_dir_all(&results_dir);
    let results_path = results_dir.join(format!("{mode}-{stamp}.json"));
    let written = serde_json::to_string_pretty(&report)
        .map(|json| std::fs::write(&results_path, json + "\n").is_ok())
        .unwrap_or(false);
    println!("{summary}");
    if written {
        println!("results: {}", results_path.display());
    }
    Ok(eval_exit_code(&results))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(
        id: &str,
        agent: Option<&str>,
        outcome: &str,
        tokens: Option<u64>,
        cost: Option<u64>,
    ) -> TaskResult {
        TaskResult {
            id: id.to_owned(),
            category: "bugfix".to_owned(),
            outcome: outcome.to_owned(),
            reason: None,
            failure_kind: None,
            trial: None,
            wall_ms: 1,
            tokens,
            cost_usd_micros: cost,
            cost_estimate_usd_micros: None,
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            num_turns: None,
            agent: agent.map(|a| a.to_owned()),
            model: None,
        }
    }

    #[test]
    fn grok_usage_json_yields_tokens_cost_and_turns() {
        // Verbatim shape from a live grok CLI `--output-format json` run.
        let stdout = r#"{
  "text": "OK",
  "stopReason": "end_turn",
  "sessionId": "01a0a130",
  "usage": {"input_tokens": 5961, "cache_read_input_tokens": 10496, "output_tokens": 34, "reasoning_tokens": 29, "total_tokens": 16491},
  "num_turns": 1,
  "total_cost_usd": 0.00590716,
  "total_cost_usd_ticks": 59071600
}"#;
        let usage = parse_grok_usage(stdout).expect("parses");
        assert_eq!(usage.tokens, Some(16491));
        assert_eq!(usage.num_turns, Some(1));
        // ticks / 10_000, truncated: 59071600 / 10_000 = 5907 micros.
        assert_eq!(usage.cost_usd_micros, Some(5907));
    }

    #[test]
    fn grok_error_object_reports_no_usage() {
        let stdout = r#"{"type":"error","message":"Couldn't set model: unknown model id"}"#;
        assert_eq!(parse_grok_usage(stdout), None);
    }

    #[test]
    fn rapid_usage_file_parses_and_null_cost_stays_unknown() {
        let usage = parse_rapid_usage_file(
            r#"{"tokens":4321,"cost_usd_micros":null,"input_tokens":4000,"cached_tokens":3200,"output_tokens":321,"tool_calls":6,"status":"Completed"}"#,
        )
        .expect("parses");
        assert_eq!(usage.tokens, Some(4321));
        assert_eq!(usage.cost_usd_micros, None, "null is unknown, never free");
        assert_eq!(usage.input_tokens, Some(4000));
        assert_eq!(usage.cached_tokens, Some(3200));
        assert_eq!(usage.output_tokens, Some(321));
        assert_eq!(parse_rapid_usage_file("not json"), None);
        // Published-rate estimate: (4000-3200)×$2/M + 3200×$0.5/M + 321×$6/M
        // = 800×2 + 3200×0.5 + 321×6 micros = 1600 + 1600 + 1926 = 5126.
        let basis = ((2.0, 0.5, 6.0), "test basis");
        assert_eq!(estimate_cost_micros(&usage, Some(basis)), Some(5126));
        // No split reported: no estimate rather than a fabricated one.
        let bare =
            parse_rapid_usage_file(r#"{"tokens":99,"cost_usd_micros":null}"#).expect("parses");
        assert_eq!(estimate_cost_micros(&bare, Some(basis)), None);
        // No matching rate basis: the estimate is UNAVAILABLE even with a
        // full split — never priced at another model's rates.
        assert_eq!(estimate_cost_micros(&usage, None), None);
        // A model field in the agent's own output is captured.
        let modeled = parse_rapid_usage_file(
            r#"{"tokens":10,"model":"grok-4.6","input_tokens":8,"output_tokens":2}"#,
        )
        .expect("parses");
        assert_eq!(modeled.model.as_deref(), Some("grok-4.6"));
    }

    // -- accounting ---------------------------------------------------------

    #[test]
    fn accounting_counts_failed_attempts_in_the_spend_denominator_is_successes() {
        // rapid: 2 passed (1000 + 3000 tokens), 1 failed (999_999), 1
        // timeout with NO usage. Total measured = 1_003_999 over 2
        // successes; successful-only average = 2000. Missing usage = 1,
        // lower_bound = true.
        let results = vec![
            task("a-1", Some("rapid"), "passed", Some(1000), None),
            task("a-2", Some("rapid"), "passed", Some(3000), None),
            task("a-3", Some("rapid"), "failed", Some(999_999), None),
        ];
        let mut timed_out = task("a-4", Some("rapid"), "failed", None, None);
        timed_out.failure_kind = Some("timeout".to_owned());
        let results = [results, vec![timed_out]].concat();
        let (report, lines) = summarize("live", &results);
        let per_agent = report["per_agent_usage"]
            .as_object()
            .expect("per-agent block");
        let tokens = &per_agent["rapid"]["tokens_per_verified_success"];
        assert_eq!(
            tokens["tokens_all_measured_attempts_per_success"].as_u64(),
            Some(501_999),
            "failed attempts count toward spend"
        );
        assert_eq!(tokens["attempts_measured"].as_u64(), Some(3));
        assert_eq!(tokens["attempts_missing_usage"].as_u64(), Some(1));
        assert_eq!(tokens["complete_coverage"].as_bool(), Some(false));
        assert_eq!(tokens["lower_bound"].as_bool(), Some(true));
        assert_eq!(
            tokens["successful_attempts_only_average"].as_u64(),
            Some(2000)
        );
        assert!(lines.contains("lower_bound=true"));
    }

    #[test]
    fn accounting_with_zero_successes_publishes_no_rate() {
        let results = vec![
            task("a-1", Some("rapid"), "failed", Some(1000), None),
            task("a-2", Some("rapid"), "failed", Some(2000), None),
        ];
        let (report, _) = summarize("live", &results);
        let rapid = &report["per_agent_usage"]["rapid"];
        assert_eq!(rapid["verified_successes"].as_u64(), Some(0));
        assert!(
            rapid.get("tokens_per_verified_success").is_none(),
            "a rate over zero successes would be a lie"
        );
        assert!(rapid["note"].as_str().unwrap().contains("undefined"));
    }

    #[test]
    fn accounting_with_no_measurements_reports_unknown_not_zero() {
        let results = vec![
            task("a-1", Some("rapid"), "passed", None, None),
            task("a-2", Some("rapid"), "failed", None, None),
        ];
        let (report, _) = summarize("live", &results);
        let rapid = &report["per_agent_usage"]["rapid"];
        assert_eq!(rapid["verified_successes"].as_u64(), Some(1));
        assert!(
            rapid.get("tokens_per_verified_success").is_none(),
            "unknown is not zero"
        );
    }

    #[test]
    fn accounting_keeps_measured_cost_and_estimate_separate_and_includes_skipped_arms() {
        let mut passed = task("g-1", Some("grok"), "passed", Some(500), Some(1000));
        passed.cost_estimate_usd_micros = Some(700);
        let mut failed = task("g-2", Some("grok"), "failed", Some(1500), Some(3000));
        failed.cost_estimate_usd_micros = Some(2100);
        let skipped = {
            let mut t = task("k-1", Some("kimi"), "skipped", None, None);
            t.failure_kind = Some("infrastructure".to_owned());
            t.reason = Some("infrastructure: competitor kimi not found on PATH".to_owned());
            t
        };
        let results = vec![passed, failed, skipped];
        let (report, _) = summarize("live", &results);
        let grok = &report["per_agent_usage"]["grok"];
        // Measured cost: (1000 + 3000) / 1 success = 4000 — the failed
        // attempt's spend is included.
        assert_eq!(
            grok["cost_per_verified_success"]["usd_micros_all_measured_attempts_per_success"]
                .as_u64(),
            Some(4000)
        );
        // Estimate kept in its own block, never summed with measured.
        assert_eq!(
            grok["cost_estimate_per_verified_success"]["usd_micros_all_measured_attempts_per_success"]
                .as_u64(),
            Some(2800)
        );
        // The requested-but-absent arm is IN the report.
        let outcomes = &report["per_agent_outcomes"];
        assert_eq!(outcomes["kimi"]["skipped"].as_u64(), Some(1));
        assert_eq!(outcomes["kimi"]["total"].as_u64(), Some(1));
    }

    #[test]
    fn repeated_trials_report_variation_and_flag_flaky_tasks() {
        let attempt = |trial: u64, passed: bool| {
            let mut t = task(
                "a-1",
                Some("rapid"),
                if passed { "passed" } else { "failed" },
                Some(500),
                None,
            );
            t.trial = Some(trial);
            t
        };
        let results = vec![
            attempt(0, true),
            attempt(1, false),
            attempt(2, true),
            {
                let mut t = task("a-2", Some("rapid"), "failed", Some(400), None);
                t.trial = Some(0);
                t
            },
            {
                let mut t = task("a-2", Some("rapid"), "failed", Some(400), None);
                t.trial = Some(1);
                t
            },
        ];
        let (report, lines) = summarize("live", &results);
        let variation = &report["variation"]["rapid"];
        assert_eq!(variation["trials"].as_u64(), Some(3));
        assert_eq!(variation["per_task_passes"]["a-1"].as_u64(), Some(2));
        assert_eq!(variation["per_task_passes"]["a-2"].as_u64(), Some(0));
        assert_eq!(variation["flaky_tasks"][0].as_str(), Some("a-1"));
        assert_eq!(variation["never_passed"][0].as_str(), Some("a-2"));
        assert!(lines.contains("VARIATION") && lines.contains("a-1"));
    }

    // -- gate ---------------------------------------------------------------

    #[test]
    fn exit_gate_fails_empty_failed_or_skipped_runs() {
        assert_eq!(eval_exit_code(&[]), 1, "empty results cannot pass");
        assert_eq!(eval_exit_code(&[task("a", None, "passed", None, None)]), 0);
        assert_eq!(eval_exit_code(&[task("a", None, "failed", None, None)]), 1);
        let mut skipped = task("a", None, "skipped", None, None);
        skipped.failure_kind = Some("infrastructure".to_owned());
        assert_eq!(eval_exit_code(&[skipped]), 1, "all-skipped cannot pass");
        assert_eq!(
            eval_exit_code(&[
                task("a", None, "passed", None, None),
                task("b", None, "skipped", None, None)
            ]),
            1,
            "an incomplete comparison cannot pass"
        );
    }

    // -- typed classification ----------------------------------------------

    #[test]
    fn timeouts_and_agent_failures_are_failures_never_skips() {
        assert!(!RunFailure::Timeout.is_harness_skip());
        assert_eq!(RunFailure::Timeout.kind(), "timeout");
        let nonzero = RunFailure::AgentNonZero {
            snippet: "[stderr] hits beyond offset skipped: 0".to_owned(),
        };
        assert!(
            !nonzero.is_harness_skip(),
            "stderr text never reclassifies a failure"
        );
        assert_eq!(nonzero.kind(), "agent_non_zero");
        assert!(RunFailure::Vacuous.is_harness_skip());
        assert!(RunFailure::Materialize("disk full".into()).is_harness_skip());
        assert!(RunFailure::PreVerifyError("python3 missing".into()).is_harness_skip());
    }

    // -- supervised subprocess execution ------------------------------------

    fn noisy_python(seconds: u64, megabytes: u64) -> Command {
        let mut command = Command::new("python3");
        command.arg("-c").arg(format!(
            r#"import sys, time
for i in range({megabytes} * 64):
    sys.stdout.write("x" * 16384)
    sys.stderr.write("y" * 16384)
    if i % 8 == 0:
        sys.stdout.flush(); sys.stderr.flush()
sys.stdout.flush(); sys.stderr.flush()
print("DONE-STDOUT")
print("DONE-STDERR", file=sys.stderr)
time.sleep({seconds})
"#
        ));
        command
    }

    #[test]
    fn verbose_output_cannot_manufacture_a_timeout() {
        // 4 MiB to EACH stream. Under the old poll-then-read runner this
        // blocks at the 64 KiB pipe buffer and reports an artificial
        // timeout; with concurrent draining it completes.
        let started = std::time::Instant::now();
        let out = run_supervised(
            &mut noisy_python(0, 4),
            None,
            Duration::from_secs(60),
            1024 * 1024,
        )
        .expect("supervised run");
        assert!(out.success, "verbose child completed");
        assert!(!out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(55));
        assert!(out.stdout.contains("DONE-STDOUT"));
        assert!(out.stderr.contains("DONE-STDERR"));
    }

    #[test]
    fn retained_output_is_bounded_and_keeps_the_tail() {
        let mut command = Command::new("python3");
        // Bytes through the binary buffer: text-mode stdout would turn the
        // newline into `\r\n` on Windows and the exact total below would be
        // one byte off there.
        command.arg("-c").arg(
            r#"import sys
out = sys.stdout.buffer
out.write(b"START-MARKER\n")
for i in range(64):
    out.write(b"x" * 16384)
out.write(b"END-MARKER")
out.flush()
"#,
        );
        let out = run_supervised(&mut command, None, Duration::from_secs(60), 8 * 1024)
            .expect("supervised run");
        assert!(out.success);
        assert_eq!(
            out.stdout_total,
            64 * 16384 + "START-MARKER\n".len() as u64 + "END-MARKER".len() as u64
        );
        assert!(
            out.stdout_dropped > 0,
            "excess bytes were consumed, not kept"
        );
        assert!(out.stdout.len() <= 8 * 1024, "retained output is bounded");
        assert!(
            out.stdout.contains("END-MARKER"),
            "the tail is what is kept"
        );
        assert!(!out.stdout.contains("START-MARKER"));
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        // The child spawns a grandchild that sleeps for a long time; both
        // carry a unique marker. The supervised run times out at 2s; then
        // NEITHER process may survive.
        let unique = format!("rapidlm-eval-group-{}", std::process::id());
        let mut command = Command::new("python3");
        command.arg("-c").arg(format!(
            r#"import subprocess
subprocess.run(["python3", "-c", "import time; time.sleep(300)  # {unique}-grandchild"])
print("{unique}-child-after")"#
        ));
        let started = std::time::Instant::now();
        let out = run_supervised(&mut command, None, Duration::from_secs(2), 4096)
            .expect("supervised run");
        assert!(out.timed_out, "the run was killed at the ceiling");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "kill returned promptly"
        );
        assert!(!out.success);
        // Both markers must be gone. Retry briefly: SIGKILL delivery to the
        // group is immediate, but reap latency can leave zombies for a few
        // hundred ms. The `[g]` character class keeps the probe from
        // matching ITS OWN wrapper shell — `sh -c "pgrep -f X"` carries X
        // in its argv, and without the class pgrep reports the probe
        // itself as a survivor (the flake this test once showed on CI).
        let mut survived = Vec::new();
        for _ in 0..50 {
            survived = test_fixtures::processes_mentioning(&unique);
            if survived.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            survived.is_empty(),
            "process-group members survived the timeout: {survived:?}"
        );
    }

    #[test]
    fn stdin_is_delivered_without_blocking_the_runner() {
        let mut command = Command::new("cat");
        let out = run_supervised(
            &mut command,
            Some(b"hello stdin"),
            Duration::from_secs(10),
            4096,
        )
        .expect("supervised run");
        assert!(out.success);
        assert_eq!(out.stdout, "hello stdin");
    }

    // -- grading pipeline (negative + positive controls) ---------------------

    /// Materialize `task` in a fresh temp dir and return its path.
    fn scratch_for(task: &BenchTask, tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-grading-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        materialize(&dir, task).expect("materialize");
        dir
    }

    fn tests_like_task() -> BenchTask {
        // Mirrors the shape of a real `tests` task: agent writes
        // test_calc.py; the implementation is protected; one mutant per
        // requested function.
        BenchTask {
            id: "tests-ctl".to_owned(),
            category: "tests".to_owned(),
            prompt: "write tests".to_owned(),
            setup: vec![(
                "calc.py".to_owned(),
                "def add(a, b):\n    return a + b\n\ndef neg(x):\n    return -x\n".to_owned(),
            )],
            gold: vec![(String::new(), String::new())],
            verify: "python3 -B test_calc.py".to_owned(),
            verify_fails_before: true,
            protected: vec!["calc.py".to_owned()],
            mutants: vec![
                (
                    "calc.py".to_owned(),
                    "def add(a, b):\n    return a - b\n\ndef neg(x):\n    return -x\n".to_owned(),
                ),
                (
                    "calc.py".to_owned(),
                    "def add(a, b):\n    return a + b\n\ndef neg(x):\n    return x\n".to_owned(),
                ),
            ],
        }
    }

    fn write_scratch(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write scratch file");
    }

    #[test]
    fn grading_positive_control_gold_submission_passes() {
        let task = tests_like_task();
        let dir = scratch_for(&task, "gold");
        // The anti-vacuity gate runs BEFORE any submission exists: the bare
        // module must fail the judge.
        assert_eq!(prechange_check(&dir, &task), Ok(()));
        // Simulate the gold submission: meaningful tests covering BOTH
        // requested functions, each able to kill its mutant.
        write_scratch(
            &dir,
            "test_calc.py",
            "from calc import add, neg\nassert add(4, 2) == 6\nassert add(-1, 0) == -1\nassert neg(5) == -5\nassert neg(-2) == 2\nprint('OK')\n",
        );
        assert_eq!(grade_submission(&dir, &task), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_test_file_is_rejected_by_mutation_check() {
        let task = tests_like_task();
        let dir = scratch_for(&task, "empty");
        // The demonstrated grading defect: `python3 -B test_calc.py` with an
        // EMPTY test file exits 0. The mutation check must reject it.
        write_scratch(&dir, "test_calc.py", "");
        assert_eq!(
            grade_submission(&dir, &task),
            Err(GradeFailure::MutationSurvived {
                file: "calc.py".to_owned()
            }),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn weakened_assertions_are_rejected() {
        let task = tests_like_task();
        let dir = scratch_for(&task, "weakened");
        // Tests `add` but says nothing about `neg`: the neg mutant survives.
        write_scratch(
            &dir,
            "test_calc.py",
            "from calc import add\nassert add(4, 2) == 6\nassert add(-1, 0) == -1\nprint('OK')\n",
        );
        assert_eq!(
            grade_submission(&dir, &task),
            Err(GradeFailure::MutationSurvived {
                file: "calc.py".to_owned()
            }),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hard_coded_assertion_free_output_is_rejected() {
        let task = tests_like_task();
        let dir = scratch_for(&task, "hardcoded");
        // A "test" that only prints: every mutant survives it.
        write_scratch(&dir, "test_calc.py", "print('OK')\n");
        assert_eq!(
            grade_submission(&dir, &task),
            Err(GradeFailure::MutationSurvived {
                file: "calc.py".to_owned()
            }),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampering_with_a_protected_test_file_is_rejected() {
        // Bugfix-shaped task: the test file is the judge AND is protected.
        let task = BenchTask {
            id: "bugfix-ctl".to_owned(),
            category: "bugfix".to_owned(),
            prompt: "fix add".to_owned(),
            setup: vec![
                (
                    "calc.py".to_owned(),
                    "def add(a, b):\n    return a - b\n".to_owned(),
                ),
                (
                    "test_calc.py".to_owned(),
                    "from calc import add\nassert add(4, 2) == 6\nprint('OK')\n".to_owned(),
                ),
            ],
            gold: vec![(
                "calc.py".to_owned(),
                "def add(a, b):\n    return a + b\n".to_owned(),
            )],
            verify: "python3 -B test_calc.py".to_owned(),
            verify_fails_before: true,
            protected: vec!["test_calc.py".to_owned()],
            mutants: vec![],
        };
        let dir = scratch_for(&task, "tamper");
        assert_eq!(
            prechange_check(&dir, &task),
            Ok(()),
            "the task is not vacuous"
        );
        // The agent "solves" the task by weakening the test instead.
        write_scratch(&dir, "test_calc.py", "print('OK')\n");
        assert_eq!(
            grade_submission(&dir, &task),
            Err(GradeFailure::Integrity {
                path: "test_calc.py".to_owned()
            }),
        );
        // Deleting the judge is the same violation.
        std::fs::remove_file(dir.join("test_calc.py")).expect("remove");
        assert_eq!(
            grade_submission(&dir, &task),
            Err(GradeFailure::Integrity {
                path: "test_calc.py".to_owned()
            }),
        );
        // The honest fix passes.
        write_scratch(
            &dir,
            "test_calc.py",
            "from calc import add\nassert add(4, 2) == 6\nprint('OK')\n",
        );
        write_scratch(&dir, "calc.py", "def add(a, b):\n    return a + b\n");
        assert_eq!(grade_submission(&dir, &task), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hard_coding_a_protected_runner_is_rejected() {
        // Recovery-shaped task: run.sh is the harness file and is protected;
        // replacing the pipeline with `echo PIPELINE-OK` is tampering.
        let task = BenchTask {
            id: "recovery-ctl".to_owned(),
            category: "recovery".to_owned(),
            prompt: "repair pipeline".to_owned(),
            setup: vec![
                ("pipeline.py".to_owned(), "raise SystemExit(1)\n".to_owned()),
                ("run.sh".to_owned(), "python3 -B pipeline.py\n".to_owned()),
            ],
            gold: vec![("pipeline.py".to_owned(), "print('ok')\n".to_owned())],
            verify: "sh run.sh".to_owned(),
            verify_fails_before: true,
            protected: vec!["run.sh".to_owned()],
            mutants: vec![],
        };
        let dir = scratch_for(&task, "hardcode-runsh");
        write_scratch(&dir, "run.sh", "echo PIPELINE-OK\n");
        assert_eq!(
            grade_submission(&dir, &task),
            Err(GradeFailure::Integrity {
                path: "run.sh".to_owned()
            }),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mirrors `eval/suite/errors-deploy-pipeline.json`: a protected
    /// pipeline script whose judge must honor the pipeline's OWN exit
    /// status (never a downstream grep's) and independently validate the
    /// artifacts the pipeline is supposed to produce.
    fn deploy_pipeline_like_task() -> BenchTask {
        BenchTask {
            id: "errors-deploy-pipeline".to_owned(),
            category: "errors".to_owned(),
            prompt: "fix the deploy pipeline".to_owned(),
            setup: vec![
                (
                    "deploy.sh".to_owned(),
                    "set -e\npython3 tools/migrate.py\npython3 tools/seed.py\npython3 tools/report.py\necho DEPLOY-OK\n".to_owned(),
                ),
                (".env".to_owned(), "SEED_TOKEN=abc123\n".to_owned()),
                (
                    "tools/migrate.py".to_owned(),
                    "import json\n\ndef run():\n    with open(\"migrations/applied.json\", \"w\") as f:\n        json.dump([\"0001-init\"], f)\n\nif __name__ == \"__main__\":\n    run()\n    print(\"migrate: ok\")\n".to_owned(),
                ),
                (
                    "tools/seed.py".to_owned(),
                    "import json\nimport os\n\ndef run():\n    token = os.environ[\"SEED_TOKEN\"]\n    with open(\"seed/manifest.json\", \"w\") as f:\n        json.dump({\"token\": token}, f)\n\nif __name__ == \"__main__\":\n    run()\n    print(\"seed: ok\")\n".to_owned(),
                ),
                (
                    "tools/report.py".to_owned(),
                    "def run():\n    with open(\"Data/summary.csv\") as f:\n        rows = f.read().count(\"\\n\")\n    print(\"report: %d rows\" % rows)\n\nif __name__ == \"__main__\":\n    run()\n".to_owned(),
                ),
                ("data/summary.csv".to_owned(), "id,name\n1,a\n2,b\n".to_owned()),
            ],
            gold: vec![
                (
                    "tools/migrate.py".to_owned(),
                    "import json\nimport os\n\ndef run():\n    os.makedirs(\"migrations\", exist_ok=True)\n    with open(\"migrations/applied.json\", \"w\") as f:\n        json.dump([\"0001-init\"], f)\n\nif __name__ == \"__main__\":\n    run()\n    print(\"migrate: ok\")\n".to_owned(),
                ),
                (
                    "tools/seed.py".to_owned(),
                    "import json\nimport os\n\ndef _token():\n    token = os.environ.get(\"SEED_TOKEN\")\n    if token:\n        return token\n    with open(\".env\") as f:\n        for line in f:\n            if line.startswith(\"SEED_TOKEN=\"):\n                return line.strip().split(\"=\", 1)[1]\n    raise SystemExit(\"no SEED_TOKEN\")\n\ndef run():\n    os.makedirs(\"seed\", exist_ok=True)\n    with open(\"seed/manifest.json\", \"w\") as f:\n        json.dump({\"token\": _token()}, f)\n\nif __name__ == \"__main__\":\n    run()\n    print(\"seed: ok\")\n".to_owned(),
                ),
                (
                    "tools/report.py".to_owned(),
                    "def run():\n    with open(\"data/summary.csv\") as f:\n        rows = f.read().count(\"\\n\")\n    print(\"report: %d rows\" % rows)\n\nif __name__ == \"__main__\":\n    run()\n".to_owned(),
                ),
            ],
            // rm -rf first: the judge validates THIS run's artifacts, never
            // outputs left behind by an earlier verification pass.
            verify: "rm -rf migrations seed\nout=$(sh deploy.sh 2>&1) || { printf '%s\\n' \"$out\" >&2; exit 1; }\nprintf '%s\\n' \"$out\" | grep -q DEPLOY-OK || { printf '%s\\n' \"$out\" >&2; exit 1; }\ngrep -q 0001-init migrations/applied.json || exit 1\ngrep -q abc123 seed/manifest.json || exit 1\n".to_owned(),
            verify_fails_before: true,
            protected: vec!["deploy.sh".to_owned(), ".env".to_owned()],
            mutants: vec![
                (
                    "tools/migrate.py".to_owned(),
                    "raise SystemExit(\"migrate broken\")\n".to_owned(),
                ),
                (
                    "tools/migrate.py".to_owned(),
                    "print(\"migrate: ok\")\n".to_owned(),
                ),
                (
                    "tools/seed.py".to_owned(),
                    "raise SystemExit(\"seed broken\")\n".to_owned(),
                ),
                (
                    "tools/report.py".to_owned(),
                    "raise SystemExit(\"report broken\")\n".to_owned(),
                ),
            ],
        }
    }

    #[test]
    fn deploy_pipeline_gold_submission_passes_all_mutants_rejected() {
        let task = deploy_pipeline_like_task();
        let dir = scratch_for(&task, "deploy-gold");
        assert_eq!(prechange_check(&dir, &task), Ok(()));
        for (path, contents) in &task.gold {
            write_scratch(&dir, path, contents);
        }
        // Runs every mutant internally: a broken or hard-coded tool must
        // fail the judge or the grade is MutationSurvived.
        assert_eq!(grade_submission(&dir, &task), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deploy_pipeline_print_then_exit_one_is_rejected() {
        // The demonstrated grading defect: migrate prints DEPLOY-OK and
        // exits 1. The old judge `sh deploy.sh | grep DEPLOY-OK` reported
        // grep's status and accepted this. The pipeline's own failure must
        // now fail the verification command.
        let task = deploy_pipeline_like_task();
        let dir = scratch_for(&task, "deploy-cheat-exit1");
        write_scratch(
            &dir,
            "tools/migrate.py",
            "print(\"DEPLOY-OK\")\nraise SystemExit(1)\n",
        );
        let verdict = grade_submission(&dir, &task);
        assert!(
            matches!(verdict, Err(GradeFailure::Verification { .. })),
            "the broken pipeline must be rejected, got {verdict:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deploy_pipeline_exit_zero_without_artifacts_is_rejected() {
        // A subtler cheat: every step exits 0 and DEPLOY-OK prints, but
        // migrate never writes its artifact. Exit status alone cannot catch
        // this — the judge validates the artifacts independently.
        let task = deploy_pipeline_like_task();
        let dir = scratch_for(&task, "deploy-cheat-noart");
        for (path, contents) in &task.gold {
            write_scratch(&dir, path, contents);
        }
        write_scratch(
            &dir,
            "tools/migrate.py",
            "print(\"DEPLOY-OK\")\nprint(\"migrate: ok\")\n",
        );
        let verdict = grade_submission(&dir, &task);
        assert!(
            matches!(verdict, Err(GradeFailure::Verification { .. })),
            "a deployment without its artifacts must be rejected, got {verdict:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vacuous_tasks_and_broken_environments_are_distinguished() {
        // Vacuous: the verify passes with no changes at all.
        let vacuous = BenchTask {
            id: "vac-ctl".to_owned(),
            category: "bugfix".to_owned(),
            prompt: "nothing".to_owned(),
            setup: vec![("m.py".to_owned(), "x = 1\n".to_owned())],
            gold: vec![],
            verify: "true".to_owned(),
            verify_fails_before: true,
            protected: vec![],
            mutants: vec![],
        };
        let dir = scratch_for(&vacuous, "vacuous");
        assert_eq!(prechange_check(&dir, &vacuous), Err(GradeFailure::Vacuous));
        let _ = std::fs::remove_dir_all(&dir);
        // Broken environment: the verify cannot EXECUTE — infrastructure,
        // not a failing submission.
        let broken = BenchTask {
            verify: "definitely-not-a-real-interpreter-3b7f test_m.py".to_owned(),
            ..vacuous.clone()
        };
        let dir = scratch_for(&broken, "broken");
        match prechange_check(&dir, &broken) {
            Err(GradeFailure::Infrastructure { .. }) => {}
            other => panic!("expected infrastructure, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suite_digest_is_stable_and_sensitive_to_changes() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-digest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("a.json"), "{\"id\":\"a\"}").expect("write");
        std::fs::write(dir.join("b.json"), "{\"id\":\"b\"}").expect("write");
        let first = suite_digest(&dir).expect("digest");
        assert_eq!(suite_digest(&dir).expect("digest"), first, "stable");
        std::fs::write(dir.join("b.json"), "{\"id\":\"b2\"}").expect("write");
        assert_ne!(suite_digest(&dir).expect("digest"), first, "sensitive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_requested_arm_is_planned_even_when_absent() {
        let arms = plan_arms(Path::new("/nonexistent-self"));
        let names: Vec<&str> = arms.iter().map(|arm| arm.name.as_str()).collect();
        assert!(names.contains(&"rapid"));
        assert_eq!(arms[0].name, "rapid");
        assert!(arms[0].state.is_ok(), "our own arm is always ready");
        for recipe in RECIPES {
            assert!(
                names.contains(&recipe.name),
                "requested competitor {} must appear in the plan",
                recipe.name
            );
        }
        // Every arm carries its pin so provenance records what was required.
        for arm in &arms {
            assert!(!arm.pinned_version.is_empty());
        }
    }

    #[test]
    fn load_suite_parses_protected_and_mutants() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-suite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("t.json"),
            r#"{"id":"t","category":"tests","prompt":"p","setup":{"m.py":"x = 1\n"},
                "gold":{},"verify":"true","verify_fails_before":true,
                "protected":["m.py"],
                "mutants":[{"file":"m.py","contents":"x = 2\n"}]}"#,
        )
        .expect("write");
        let tasks = load_suite(&dir).expect("load");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].protected, vec!["m.py".to_owned()]);
        assert_eq!(
            tasks[0].mutants,
            vec![("m.py".to_owned(), "x = 2\n".to_owned())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
