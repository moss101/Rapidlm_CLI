//! `rapid run` — the public workflow execution path.
//!
//! A playbook file is loaded, validated through the scheduler's own compiler
//! (bounds, unique keys, acyclic dependencies, single connected entry — the
//! one authority for what a valid workflow is), and then *executed* here:
//!
//! - dependent and independent steps, with bounded parallelism;
//! - agent steps run a real turn (`run_live_exec`, the same assembly the
//!   headless `rapid exec` path uses — model, tools, trust, permission
//!   lattice, budget);
//! - verification steps run a shell command and require exit 0;
//! - approval and question steps pause the run on the same durable
//!   pending-approval machinery the interactive TUI uses, and `rapid run
//!   --resume` continues after a human decision — including from a
//!   restarted process (the run state is a file under `.rapidlm/runs/`);
//! - monitor steps run a bounded shell command once (a repeat-until-
//!   success poll is not implemented — they run like process steps);
//! - failed steps retry in place; completed steps are never replayed (their
//!   recorded outcome is the guard against repeating external effects);
//! - steps that declare `watch` globs are **invalidated on resume** when the
//!   files they cover changed since their evidence was recorded — stale
//!   verification is never reported as fresh;
//! - the final report distinguishes "the run finished" from "the work is
//!   verified": `verified: true` requires every verification step to have
//!   run *in this invocation* against the current tree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kernel::KernelClient as _;
use protocol::{ArtifactId, GraphId};
use scheduler::graph::RuntimeGraph;
use scheduler::kinds::NodeKind;
use scheduler::playbook::{PlaybookStep, PlaybookTemplate, compile};

use crate::approvals::{ApprovalRequest, ApprovalSink, LedgerApprovalSink};

/// Concurrent step ceiling. A playbook's parallelism is bounded — an
/// unbounded fan-out of agent turns would multiply cost and machine load
/// with one typo.
pub const DEFAULT_MAX_PARALLEL: usize = 4;

/// Byte cap on one step's recorded result summary in the run state.
const MAX_RESULT_BYTES: usize = 4 * 1024;

/// Wall-clock ceiling for one agent step, when the playbook sets none.
pub(crate) const DEFAULT_STEP_TIMEOUT_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// Playbook file format (superset of what `rapid playbook-compile` reads)
// ---------------------------------------------------------------------------

/// One executable step, parsed from the playbook file. `watch`/`prompt`/
/// `command`/`timeout` are execution fields the compiler ignores but the
/// runner requires for the kinds that need them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub key: String,
    pub kind: NodeKind,
    pub label: String,
    pub depends_on: Vec<String>,
    pub budget_tokens: u64,
    pub max_attempts: u32,
    /// Task text for agent steps (`prompt`); defaults to the label.
    pub prompt: Option<String>,
    /// Shell command for verification/process/monitor steps.
    pub command: Option<String>,
    /// Glob list defining the evidence scope for verification steps.
    pub watch: Vec<String>,
    /// Wall-clock ceiling in seconds for process/monitor steps.
    pub timeout_secs: Option<u64>,
    /// The question a `ask_user` step asks.
    pub question: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybookFile {
    pub name: String,
    pub steps: Vec<Step>,
}

/// Typed load/validate failure — the compiler's own errors plus file errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowError {
    Io(String),
    Json(String),
    Compiler(scheduler::playbook::PlaybookError),
    /// A step kind is missing the execution field it requires (e.g. a
    /// `verification` step without a `command`).
    InvalidStep {
        key: String,
        reason: String,
    },
    Run(String),
}

impl core::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::Compiler(err) => write!(f, "{err}"),
            Self::InvalidStep { key, reason } => write!(f, "step '{key}': {reason}"),
            Self::Run(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WorkflowError {}

fn kind_from_str(raw: &str) -> Option<NodeKind> {
    // The same names `rapid playbook-compile` accepts, snake_case per the
    // node catalog.
    let kind = match raw {
        "goal" => NodeKind::Goal,
        "criterion" => NodeKind::Criterion,
        "plan" => NodeKind::Plan,
        "task" => NodeKind::Task,
        "agent" => NodeKind::Agent,
        "context_query" => NodeKind::ContextQuery,
        "context_packet" => NodeKind::ContextPacket,
        "tool" => NodeKind::ToolInvocation,
        "process" => NodeKind::Process,
        "monitor" => NodeKind::Monitor,
        "approval" => NodeKind::Approval,
        "ask_user" => NodeKind::AskUser,
        "artifact" => NodeKind::Artifact,
        "claim" => NodeKind::Claim,
        "verification" => NodeKind::Verification,
        "join" => NodeKind::Join,
        "human_control" => NodeKind::HumanControl,
        _ => return None,
    };
    Some(kind)
}

/// Load a playbook file: parse, map to steps, and validate through the
/// scheduler compiler (which checks bounds, uniqueness, acyclicity and
/// connectivity). Returns the file plus the compiled graph — the graph is
/// stored in the run state as the validated plan-of-record.
pub fn load_playbook(path: &Path) -> Result<(PlaybookFile, RuntimeGraph), WorkflowError> {
    let bytes = std::fs::read(path).map_err(|err| WorkflowError::Io(err.to_string()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|err| WorkflowError::Json(err.to_string()))?;
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("playbook")
        .to_owned();
    let steps_value = value
        .get("steps")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| WorkflowError::Json("playbook needs a non-empty `steps` array".into()))?;
    let mut steps = Vec::new();
    for step in steps_value {
        let key = step
            .get("key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| WorkflowError::Json("every step needs a `key`".into()))?
            .to_owned();
        let kind_raw = step
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| WorkflowError::InvalidStep {
                key: key.clone(),
                reason: "missing `kind`".to_owned(),
            })?;
        let kind = kind_from_str(kind_raw).ok_or_else(|| WorkflowError::InvalidStep {
            key: key.clone(),
            reason: format!("unknown kind '{kind_raw}'"),
        })?;
        let field = |name: &str| {
            step.get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let list = |name: &str| {
            step.get(name)
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        // An empty `label` or `command` is the same as none: a label falls
        // back to the key, and a command-bearing step without a real
        // command is rejected below rather than run as `""`.
        let field = |name: &str| field(name).filter(|value| !value.is_empty());
        steps.push(Step {
            key: key.clone(),
            kind,
            label: field("label").unwrap_or_else(|| key.clone()),
            depends_on: list("depends_on"),
            budget_tokens: step
                .get("budget_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            max_attempts: step
                .get("max_attempts")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n.max(1) as u32)
                .unwrap_or(3),
            prompt: field("prompt"),
            command: field("command"),
            watch: list("watch"),
            timeout_secs: step.get("timeout_secs").and_then(serde_json::Value::as_u64),
            question: field("question"),
        });
    }
    // Validate through the compiler — the single authority for playbook
    // validity. Any compile error is the load error.
    let mut template = PlaybookTemplate::new(name.clone());
    for step in &steps {
        template = template.push(
            PlaybookStep::new(step.key.clone(), step.kind, step.label.clone())
                .with_dependencies(step.depends_on.clone())
                .with_budget(step.budget_tokens)
                .with_max_attempts(step.max_attempts),
        );
    }
    let graph = compile(&template, GraphId::new()).map_err(WorkflowError::Compiler)?;
    // Kind-specific requirements the compiler does not know about.
    for step in &steps {
        match step.kind {
            NodeKind::Verification | NodeKind::Process | NodeKind::Monitor => {
                if step.command.is_none() {
                    return Err(WorkflowError::InvalidStep {
                        key: step.key.clone(),
                        reason: format!("a {:?} step needs a `command`", step.kind),
                    });
                }
            }
            NodeKind::AskUser if step.question.is_none() => {
                return Err(WorkflowError::InvalidStep {
                    key: step.key.clone(),
                    reason: "an ask_user step needs a `question`".to_owned(),
                });
            }
            _ => {}
        }
    }
    Ok((PlaybookFile { name, steps }, graph))
}

// ---------------------------------------------------------------------------
// Run state (the durable part)
// ---------------------------------------------------------------------------

/// Terminal/interim outcome of one step, persisted after every transition.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StepState {
    Pending,
    Running,
    WaitingHuman { wait_token: String },
    Succeeded,
    Failed { reason: String, attempts: u32 },
    Cancelled,
}

/// The whole run: what the resume path replays.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RunState {
    pub run_id: String,
    pub playbook_name: String,
    pub playbook_path: String,
    /// Step keys in declaration order (entry first, mirroring the compiler).
    pub keys: Vec<String>,
    /// Per-step live state, keyed by step key.
    pub steps: BTreeMap<String, StepState>,
    /// Bounded result text per step (agent terminal output, command output).
    pub results: BTreeMap<String, String>,
    /// Evidence digest per completed step with `watch` globs: the hash of
    /// the watched files when the step's evidence was recorded. A resume
    /// that sees a different hash invalidates the step and its dependents.
    pub evidence: BTreeMap<String, String>,
    /// Verification steps that ran against the current tree in the latest
    /// invocation — the fresh-evidence half of `verified`.
    pub fresh_verification: BTreeMap<String, bool>,
    /// Attempts consumed per step. Tracked separately from the step state
    /// because marking a step `Running` for a retry must not erase the
    /// count its `Failed` entry carried.
    #[serde(default)]
    pub attempts: BTreeMap<String, u32>,
    /// Pending wait recorded for a paused run (the approval machinery's
    /// token; resolution lives in the ledger).
    pub paused_on: Option<String>,
    /// Present when the run executes under `orchestration.mode = verified`:
    /// the graph and ledger session its transitions were appended to and
    /// whether the supervisor accepted. Absent (and never written) on the
    /// default path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<crate::workflow_verified::OrchestrationRecord>,
}

impl RunState {
    pub fn new(run_id: String, playbook: &PlaybookFile, playbook_path: &Path) -> Self {
        Self {
            run_id,
            playbook_name: playbook.name.clone(),
            playbook_path: playbook_path.display().to_string(),
            keys: playbook.steps.iter().map(|s| s.key.clone()).collect(),
            steps: playbook
                .steps
                .iter()
                .map(|s| (s.key.clone(), StepState::Pending))
                .collect(),
            results: BTreeMap::new(),
            evidence: BTreeMap::new(),
            fresh_verification: BTreeMap::new(),
            attempts: BTreeMap::new(),
            paused_on: None,
            orchestration: None,
        }
    }
}

/// Load a run state, applying watch-glob invalidation: any completed step
/// whose watched files changed since its evidence was recorded — and every
/// step downstream of it — is reset to `Pending` with its recorded result
/// dropped. Stale verification is never carried forward as fresh.
pub fn load_run(
    root: &Path,
    playbook: &PlaybookFile,
    run_id: &str,
) -> Result<RunState, WorkflowError> {
    let path = run_state_path(root, run_id);
    let bytes = std::fs::read(&path).map_err(|err| WorkflowError::Io(err.to_string()))?;
    let mut state: RunState =
        serde_json::from_slice(&bytes).map_err(|err| WorkflowError::Json(err.to_string()))?;
    let steps_by_key: BTreeMap<&str, &Step> =
        playbook.steps.iter().map(|s| (s.key.as_str(), s)).collect();
    let mut invalidated: Vec<String> = Vec::new();
    for (key, entry) in steps_by_key.iter() {
        if entry.watch.is_empty() {
            continue;
        }
        let Some(recorded) = state.evidence.get(*key) else {
            continue;
        };
        if state.steps.get(*key) != Some(&StepState::Succeeded) {
            continue;
        }
        if evidence_digest(root, &entry.watch) != *recorded {
            invalidated.push((*key).to_owned());
        }
    }
    // Invalidate the changed steps and everything downstream of them.
    let mut queue = invalidated.clone();
    while let Some(key) = queue.pop() {
        if state.steps.get(&key) == Some(&StepState::Succeeded) {
            state.steps.insert(key.clone(), StepState::Pending);
            state.results.remove(&key);
            state.evidence.remove(&key);
            state.fresh_verification.remove(&key);
        }
        for step in &playbook.steps {
            if step.depends_on.contains(&key)
                && state.steps.get(&step.key) == Some(&StepState::Succeeded)
            {
                queue.push(step.key.clone());
            }
        }
    }
    Ok(state)
}

fn run_state_path(root: &Path, run_id: &str) -> PathBuf {
    root.join(".rapidlm")
        .join("runs")
        .join(format!("{run_id}.json"))
}

/// Persist atomically (write sibling + rename) so a crash mid-write never
/// corrupts the only copy of the run state.
pub fn save_run(root: &Path, state: &RunState) -> Result<(), WorkflowError> {
    let path = run_state_path(root, &state.run_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| WorkflowError::Io(err.to_string()))?;
    }
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|err| WorkflowError::Json(err.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|err| WorkflowError::Io(err.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|err| WorkflowError::Io(err.to_string()))
}

/// SHA-256 over every watched file's path + content, order-normalized.
/// `ArtifactId::from_bytes` is the codebase's existing content hash; the
/// digest only needs to be stable and comparable.
pub fn evidence_digest(root: &Path, globs: &[String]) -> String {
    let mut matched: Vec<PathBuf> = Vec::new();
    for pattern in globs {
        collect_glob(root, Path::new(pattern), Path::new(""), &mut matched);
    }
    matched.sort();
    matched.dedup();
    let mut bytes = Vec::new();
    for path in matched {
        bytes.extend_from_slice(path.to_string_lossy().as_bytes());
        if let Ok(content) = std::fs::read(root.join(&path)) {
            bytes.extend_from_slice(&content);
        }
        bytes.push(0);
    }
    ArtifactId::from_bytes(&bytes).to_string()
}

/// Bounded glob matching: `*` within a segment, `**` across segments.
fn collect_glob(root: &Path, pattern: &Path, prefix: &Path, out: &mut Vec<PathBuf>) {
    let Some(first) = pattern.components().next() else {
        return;
    };
    if out.len() >= 4096 {
        return;
    }
    let first = first.as_os_str().to_string_lossy().to_string();
    if first == "**" {
        // Match everything below: walk one level of directory entries.
        let rest = pattern.strip_prefix("**").unwrap_or(pattern);
        let rest = rest
            .strip_prefix(std::path::MAIN_SEPARATOR_STR)
            .unwrap_or(rest);
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                let relative = prefix.join(entry.file_name());
                if path.is_dir() {
                    collect_glob(&path, rest, &relative, out);
                    if segment_glob(rest, &relative) {
                        out.push(relative);
                    }
                } else if segment_glob(rest, &relative) {
                    out.push(relative);
                }
            }
        }
        return;
    }
    if first.contains('*') {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !glob_segment(&first, &name) {
                    continue;
                }
                let rest = pattern.strip_prefix(&first).ok();
                let child = entry.path();
                let relative = prefix.join(entry.file_name());
                match rest {
                    Some(remaining) if !remaining.as_os_str().is_empty() => {
                        let remaining = remaining
                            .strip_prefix(std::path::MAIN_SEPARATOR_STR)
                            .unwrap_or(remaining);
                        if child.is_dir() {
                            collect_glob(&child, Path::new(remaining), &relative, out);
                        }
                    }
                    _ => {
                        if child.is_file() {
                            out.push(relative);
                        }
                    }
                }
            }
        }
        return;
    }
    let child = root.join(&first);
    let rest = pattern.strip_prefix(&first).ok();
    let relative = prefix.join(&first);
    match rest {
        Some(remaining) if !remaining.as_os_str().is_empty() => {
            let remaining = remaining
                .strip_prefix(std::path::MAIN_SEPARATOR_STR)
                .unwrap_or(remaining);
            if child.is_dir() {
                collect_glob(&child, Path::new(remaining), &relative, out);
            }
        }
        _ => {
            if child.is_file() {
                out.push(relative);
            }
        }
    }
}

fn glob_segment(pattern: &str, text: &str) -> bool {
    // Iterative `*` wildcard match; `*` stays within one path segment (it
    // never matches `/`) and `?` matches one character — the shell-glob
    // contract, so `*.rs` names files, not trees.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            // `*` absorbs one more character — never `/`.
            if t[mark] == '/' {
                return false;
            }
            mark += 1;
            ti = mark;
            pi = star + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn segment_glob(pattern: &Path, relative: &Path) -> bool {
    let pattern = pattern.to_string_lossy();
    let text = relative.to_string_lossy();
    pattern
        .split('/')
        .zip(text.split('/'))
        .all(|(p, t)| glob_segment(p, t))
        && pattern.split('/').count() == text.split('/').count()
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Why the run returned. Drives the exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Every step reached a terminal state and every verification step ran
    /// fresh in this invocation.
    Verified,
    /// Every step reached a terminal state, but at least one verification
    /// step is stale or failed — finished is not verified.
    CompletedUnverified {
        unmet: Vec<String>,
    },
    /// A step failed after its retries.
    Failed {
        key: String,
        reason: String,
    },
    /// A human decision is pending; resolve and `--resume`.
    Paused {
        key: String,
        wait_token: String,
    },
    Cancelled,
    /// Verified orchestration could not record or conclude the run (the
    /// graph refused a transition, the ledger append failed, the supervisor
    /// refused). The step results reached so far are saved; nothing is
    /// accepted. Never produced on the default path.
    OrchestrationFailed {
        reason: String,
    },
}

/// One step's execution context handed to the model/command executors.
pub struct RunContext<'a> {
    pub root: &'a Path,
    pub trusted: bool,
    pub max_parallel: usize,
    /// Recorded events land here — one JSON line per transition (the
    /// `--jsonl` surface of a run).
    pub events: Option<Arc<Mutex<Vec<String>>>>,
}

pub fn new_run_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Hex only: run ids double as file names and sort by creation time.
    format!("run-{nanos:x}")
}

/// One agent step: task text → bounded terminal summary. Shared across the
/// step threads; each call resolves its own model (per-call env/config
/// reads are cheap) so steps genuinely run in parallel.
pub type AgentStepFn = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;
/// One command step: command + timeout → bounded captured output.
pub type CommandStepFn = Arc<dyn Fn(&str, u64) -> Result<String, String> + Send + Sync>;
/// Records the durable human wait for an approval/question step.
pub type HumanWaitFn = Arc<dyn Fn(&Step, &RunState) -> Result<String, String> + Send + Sync>;

/// Execute (or resume) a playbook run. The step closures are injected so
/// the CLI path and tests share this executor.
///
/// With `orchestration` (`orchestration.mode = verified`) every transition
/// is recorded on the run's Runtime Graph before the run state changes,
/// the graph must agree with the run state about what is ready, and
/// `Verified` is the supervisor's acceptance rather than the freshness
/// bookkeeping alone. Any refusal on that path ends the run as
/// [`RunOutcome::OrchestrationFailed`]. With `None` this function behaves
/// exactly as it always has.
#[allow(clippy::too_many_arguments)]
pub fn execute_run(
    playbook: &PlaybookFile,
    state: &mut RunState,
    context: &RunContext<'_>,
    run_agent_step: AgentStepFn,
    run_command_step: CommandStepFn,
    request_human: HumanWaitFn,
    cancel: &agent_runtime::CancellationToken,
    mut orchestration: Option<&mut crate::workflow_verified::VerifiedRun>,
) -> RunOutcome {
    let steps_by_key: BTreeMap<&str, &Step> =
        playbook.steps.iter().map(|s| (s.key.as_str(), s)).collect();
    let record = |events: &Option<Arc<Mutex<Vec<String>>>>, line: String| {
        if let Some(events) = events
            && let Ok(mut buffer) = events.lock()
        {
            buffer.push(line);
        }
    };
    // The graph refusing anything is the end of the run: save what is
    // recorded and report why. `verified!` is for transitions that precede
    // an effect (a start): the graph refusing means the step does not run.
    // `verified_after!` is for facts that already happened (an outcome, a
    // recorded wait): the run state records them regardless — a resume
    // must not repeat an effect because the graph could not be told — and
    // the refusal ends the run right after. Both are no-ops on the default
    // path.
    macro_rules! verified {
        ($verified:ident => $call:expr) => {
            if let Some($verified) = orchestration.as_deref_mut() {
                if let Err(err) = $call {
                    save_run_ok(context.root, state);
                    return RunOutcome::OrchestrationFailed {
                        reason: err.to_string(),
                    };
                }
            }
        };
    }
    macro_rules! verified_after {
        ($verified:ident => $call:expr) => {
            match orchestration.as_deref_mut() {
                Some($verified) => $call.err(),
                None => None,
            }
        };
    }
    let orchestration_failed =
        |state: &RunState, err: crate::workflow_verified::VerifiedRunError| {
            save_run_ok(context.root, state);
            RunOutcome::OrchestrationFailed {
                reason: err.to_string(),
            }
        };
    // Fresh means this invocation: a verification step that passed last
    // time is finished, not verified, until it runs again (`load_run`
    // already drops the entries whose watched files changed; the rest are
    // dropped here so a resume that runs nothing cannot report `verified`).
    state.fresh_verification.clear();
    match orchestration.as_deref() {
        Some(verified) => state.orchestration = Some(verified.record()),
        None => {
            // A run that started verified and continues with the mode off
            // keeps the pointer to its graph but not its verdict: nothing
            // on this path can accept.
            if let Some(record) = state.orchestration.as_mut() {
                record.state = "abandoned: resumed with orchestration.mode = off".to_owned();
                record.accepted = false;
            }
        }
    }
    // Persist the cleared freshness before the first batch, so a resume
    // that finds nothing ready still rewrites the file rather than leaving
    // `--status` reading a stale `[verified fresh]`.
    save_run_ok(context.root, state);

    loop {
        if cancel.is_cancelled() {
            return RunOutcome::Cancelled;
        }
        // Ready = pending, or failed with attempts left, whose dependencies
        // all succeeded. A dependency that failed *for good* (attempts
        // exhausted, or cancelled) cancels its dependents; one that is
        // still retrying merely keeps them waiting — the same rule the
        // Runtime Graph applies (`PredecessorSucceeded`).
        let mut ready: Vec<&Step> = Vec::new();
        for step in &playbook.steps {
            match state.steps.get(&step.key) {
                Some(StepState::Pending) => {}
                Some(StepState::Failed { .. }) if !failed_for_good(state, step) => {}
                _ => continue,
            }
            let deps_met = step
                .depends_on
                .iter()
                .all(|dep| state.steps.get(dep) == Some(&StepState::Succeeded));
            let deps_failed = step
                .depends_on
                .iter()
                .any(|dep| match state.steps.get(dep) {
                    Some(StepState::Cancelled) => true,
                    Some(StepState::Failed { .. }) => steps_by_key
                        .get(dep.as_str())
                        .is_some_and(|dep_step| failed_for_good(state, dep_step)),
                    _ => false,
                });
            if deps_failed {
                // A dependency failed for good: this step can never run.
                verified!(v => v.step_cancelled(&step.key));
                state.steps.insert(step.key.clone(), StepState::Cancelled);
                record(
                    &context.events,
                    serde_json::json!({
                        "event": "step.cancelled", "step": step.key,
                        "reason": "a dependency failed permanently",
                    })
                    .to_string(),
                );
                continue;
            }
            if deps_met {
                ready.push(step);
            }
        }

        if ready.is_empty() {
            break;
        }
        verified!(v => v.confirm_ready(playbook, &ready));

        // Human steps never run concurrently with anything: they pause the
        // whole run on the durable wait machinery. A human step in the
        // ready set is the whole batch — the other ready steps stay
        // `Pending` for the resume rather than being marked `Running` for a
        // batch that never runs them.
        let human = ready
            .iter()
            .copied()
            .find(|s| matches!(s.kind, NodeKind::Approval | NodeKind::AskUser));
        match human {
            Some(step) => ready = vec![step],
            // Bounded parallelism: take up to `max_parallel` ready steps.
            None => ready.truncate(context.max_parallel.max(1)),
        }
        // The whole batch starts on the graph before any run-state entry
        // flips: a refusal partway leaves every step `Pending`, so the
        // resume re-queues them instead of finding steps that never ran
        // marked `Running`.
        for step in &ready {
            verified!(v => v.step_started(&step.key));
        }
        for step in &ready {
            state.steps.insert(step.key.clone(), StepState::Running);
            record(
                &context.events,
                serde_json::json!({ "event": "step.started", "step": step.key, "kind": format!("{:?}", step.kind) })
                    .to_string(),
            );
        }
        save_run_ok(context.root, state);

        if let Some(step) = human {
            let question = step.question.clone().unwrap_or_else(|| step.label.clone());
            match request_human(step, state) {
                Ok(wait_token) => {
                    let refused = verified_after!(v => v.step_waiting(&step.key, &wait_token));
                    state.steps.insert(
                        step.key.clone(),
                        StepState::WaitingHuman {
                            wait_token: wait_token.clone(),
                        },
                    );
                    state.paused_on = Some(step.key.clone());
                    save_run_ok(context.root, state);
                    if let Some(err) = refused {
                        return orchestration_failed(state, err);
                    }
                    record(
                        &context.events,
                        serde_json::json!({ "event": "run.paused", "step": step.key, "wait_token": wait_token, "question": question })
                            .to_string(),
                    );
                    return RunOutcome::Paused {
                        key: step.key.clone(),
                        wait_token,
                    };
                }
                Err(reason) => {
                    let refused = verified_after!(v => v.step_failed(step, &reason, false));
                    state.attempts.insert(step.key.clone(), u32::MAX);
                    state.steps.insert(
                        step.key.clone(),
                        StepState::Failed {
                            reason: bounded(&reason),
                            attempts: u32::MAX,
                        },
                    );
                    save_run_ok(context.root, state);
                    if let Some(err) = refused {
                        return orchestration_failed(state, err);
                    }
                    return RunOutcome::Failed {
                        key: step.key.clone(),
                        reason: bounded(&reason),
                    };
                }
            }
        }

        // Run the batch. Each step executes on its own thread; results land
        // in a shared map the parent folds into the state afterwards. The
        // step closures are `Send + Sync`, so threads share them directly.
        let outcomes: Arc<Mutex<BTreeMap<String, Result<String, String>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let mut handles = Vec::new();
        for step in &ready {
            let outcomes = Arc::clone(&outcomes);
            let key = step.key.clone();
            let kind = step.kind;
            let prompt = step.prompt.clone().unwrap_or_else(|| step.label.clone());
            let command = step.command.clone().unwrap_or_default();
            let timeout = step.timeout_secs.unwrap_or(DEFAULT_STEP_TIMEOUT_SECS);
            let run_agent_step = Arc::clone(&run_agent_step);
            let run_command_step = Arc::clone(&run_command_step);
            let handle = std::thread::spawn(move || {
                let result = match kind {
                    NodeKind::Verification | NodeKind::Process | NodeKind::Monitor => {
                        run_command_step(&command, timeout)
                    }
                    _ => run_agent_step(&key, &prompt),
                };
                if let Ok(mut map) = outcomes.lock() {
                    map.insert(key, result);
                }
            });
            handles.push(handle);
        }
        for handle in handles {
            let _ = handle.join();
        }

        // Fold outcomes into the state. Every outcome is a fact that
        // already happened, so all of them are recorded before a graph
        // refusal (kept in `refused`) is allowed to end the run.
        let mut refused: Option<crate::workflow_verified::VerifiedRunError> = None;
        let folded: BTreeMap<String, Result<String, String>> =
            outcomes.lock().map(|map| map.clone()).unwrap_or_default();
        {
            for (key, result) in folded.iter() {
                let step = steps_by_key[key.as_str()];
                match result {
                    Ok(summary) => {
                        if refused.is_none() {
                            refused = verified_after!(v => v.step_succeeded(step, summary));
                        }
                        state.steps.insert(key.clone(), StepState::Succeeded);
                        state.results.insert(key.clone(), bounded(summary));
                        if !step.watch.is_empty() {
                            state
                                .evidence
                                .insert(key.clone(), evidence_digest(context.root, &step.watch));
                        }
                        if step.kind == NodeKind::Verification {
                            state.fresh_verification.insert(key.clone(), true);
                        }
                        record(
                            &context.events,
                            serde_json::json!({ "event": "step.succeeded", "step": key })
                                .to_string(),
                        );
                    }
                    Err(reason) => {
                        let attempts = state
                            .attempts
                            .entry(key.clone())
                            .and_modify(|attempts| *attempts += 1)
                            .or_insert(1);
                        let attempts = *attempts;
                        if refused.is_none() {
                            refused = verified_after!(
                                v => v.step_failed(step, reason, attempts < step.max_attempts)
                            );
                        }
                        state.steps.insert(
                            key.clone(),
                            StepState::Failed {
                                reason: bounded(reason),
                                attempts,
                            },
                        );
                        record(
                            &context.events,
                            serde_json::json!({ "event": "step.failed", "step": key, "attempts": attempts, "reason": bounded(reason) })
                                .to_string(),
                        );
                    }
                }
            }
        }
        save_run_ok(context.root, state);
        if let Some(err) = refused {
            return orchestration_failed(state, err);
        }

        // A failed step that exhausted its retries ends the run with the
        // explanation; `--retry <step>` resets it.
        for step in &playbook.steps {
            if let Some(StepState::Failed { reason, .. }) = state.steps.get(&step.key)
                && failed_for_good(state, step)
            {
                return RunOutcome::Failed {
                    key: step.key.clone(),
                    reason: reason.clone(),
                };
            }
        }
    }

    // Completion semantics: finished ≠ verified.
    let mut unmet = Vec::new();
    for step in &playbook.steps {
        if step.kind == NodeKind::Verification
            && state.fresh_verification.get(&step.key) != Some(&true)
        {
            unmet.push(step.key.clone());
        }
    }
    // Under verified orchestration the supervisor's acceptance is the
    // verdict: it is reached only when every verification requirement is
    // backed by a passing check from this invocation, and it is recorded
    // in the run state either way.
    if let Some(verified) = orchestration {
        let conclusion = match verified.conclude(playbook) {
            Ok(conclusion) => conclusion,
            Err(err) => {
                state.orchestration = Some(verified.record());
                save_run_ok(context.root, state);
                return RunOutcome::OrchestrationFailed {
                    reason: err.to_string(),
                };
            }
        };
        state.orchestration = Some(verified.record());
        save_run_ok(context.root, state);
        record(
            &context.events,
            serde_json::json!({
                "event": "run.concluded", "accepted": conclusion.accepted,
                "state": format!("{:?}", conclusion.state),
                "verdict": format!("{:?}", conclusion.verdict), "unmet": conclusion.unmet,
            })
            .to_string(),
        );
        return if conclusion.accepted {
            RunOutcome::Verified
        } else {
            let mut unmet = conclusion.unmet;
            unmet.extend(
                playbook
                    .steps
                    .iter()
                    .filter(|step| step.kind == NodeKind::Verification)
                    .filter(|step| state.fresh_verification.get(&step.key) != Some(&true))
                    .map(|step| step.key.clone()),
            );
            unmet.sort_unstable();
            unmet.dedup();
            RunOutcome::CompletedUnverified { unmet }
        };
    }
    if unmet.is_empty() {
        RunOutcome::Verified
    } else {
        RunOutcome::CompletedUnverified { unmet }
    }
}

fn save_run_ok(root: &Path, state: &RunState) {
    let _ = save_run(root, state);
}

/// Attempts a step has consumed: the run's counter, or the count its
/// `Failed` entry carries when that is higher (a human step denied or
/// refused is recorded as `u32::MAX` — no retry — on both).
pub(crate) fn attempts_used(state: &RunState, key: &str) -> u32 {
    let recorded = match state.steps.get(key) {
        Some(StepState::Failed { attempts, .. }) => *attempts,
        _ => 0,
    };
    recorded.max(state.attempts.get(key).copied().unwrap_or(0))
}

/// A `Failed` step with no attempt left. Anything else — including a
/// failed step the loop will retry — is not final.
pub(crate) fn failed_for_good(state: &RunState, step: &Step) -> bool {
    matches!(state.steps.get(&step.key), Some(StepState::Failed { .. }))
        && attempts_used(state, &step.key) >= step.max_attempts
}

fn bounded(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_RESULT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated)", &text[..end])
}

/// Reset one failed step so the next resume retries it, and un-cancel every
/// step cancelled only because this one (transitively) failed: those
/// dependents never ran, so re-queueing them repeats no external effect,
/// and without it the run could never recover — a cancelled step is skipped
/// by the readiness scan and `--retry` names one step. Completed steps are
/// refused: replaying one would repeat external effects — the at-most-once
/// contract, not a limitation.
pub fn reset_step(
    state: &mut RunState,
    playbook: &PlaybookFile,
    key: &str,
) -> Result<(), WorkflowError> {
    match state.steps.get(key) {
        Some(StepState::Failed { .. }) | Some(StepState::Cancelled) => {
            state.steps.insert(key.to_owned(), StepState::Pending);
            state.attempts.remove(key);
            uncancel_dependents(state, playbook, key);
            Ok(())
        }
        Some(StepState::Succeeded) => Err(WorkflowError::Run(format!(
            "step '{key}' succeeded; re-running it would repeat its external effects"
        ))),
        other => Err(WorkflowError::Run(format!(
            "step '{key}' is {other:?}; only a failed or cancelled step can be retried"
        ))),
    }
}

/// Set every `Cancelled` step reachable from `key` through the dependency
/// graph back to `Pending`: a step is cancelled only because a dependency
/// failed for good, so once that dependency is retried its dependents must
/// be runnable again.
fn uncancel_dependents(state: &mut RunState, playbook: &PlaybookFile, key: &str) {
    let mut queue = vec![key.to_owned()];
    while let Some(current) = queue.pop() {
        for step in &playbook.steps {
            if step.depends_on.iter().any(|dep| dep == &current)
                && state.steps.get(&step.key) == Some(&StepState::Cancelled)
            {
                state.steps.insert(step.key.clone(), StepState::Pending);
                state.attempts.remove(&step.key);
                queue.push(step.key.clone());
            }
        }
    }
}

/// The human half of a paused run: record a pending approval (question or
/// permission) through the same ledger machinery the TUI uses, and leave it
/// for `rapid run resolve`.
pub fn record_human_wait(
    root: &Path,
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
    actor: &event_ledger::event::ActorRef,
    step: &Step,
) -> Result<String, String> {
    let sink = LedgerApprovalSink::new(client.clone(), session, actor.clone(), root.to_path_buf());
    let summary = match step.kind {
        NodeKind::AskUser => step.question.clone().unwrap_or_else(|| step.label.clone()),
        _ => step.label.clone(),
    };
    let request = ApprovalRequest {
        tool: match step.kind {
            NodeKind::AskUser => "ask_user".to_owned(),
            other => format!("workflow:{}", other.as_str()),
        },
        call_id: format!("step-{}", step.key),
        summary,
        scope: step.watch.clone(),
        diff: String::new(),
    };
    ApprovalSink::request(&sink, &request)
}

/// Resolve a paused run's pending wait. The decision lands in the ledger via
/// the kernel client; `--resume` picks it up.
pub fn resolve_human_wait(
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
    actor: &event_ledger::event::ActorRef,
    wait_token: &str,
    approve: bool,
    expected_seq: u64,
) -> Result<(), String> {
    let decision = if approve {
        kernel::ApprovalDecision::Approved
    } else {
        kernel::ApprovalDecision::Denied
    };
    client_call(
        client.approve(
            kernel::ResolveApproval::new(
                session,
                expected_seq,
                decision,
                actor.clone(),
                protocol::TraceId::new(),
            )
            .with_wait_token(wait_token),
        ),
    )
}

fn client_call<T, E>(future: impl Future<Output = Result<T, E>>) -> Result<T, String>
where
    E: std::fmt::Display,
{
    let mut future = Box::pin(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match std::pin::pin!(future.as_mut()).poll(&mut cx) {
        std::task::Poll::Ready(result) => result.map_err(|err| err.to_string()),
        std::task::Poll::Pending => Err("kernel did not answer immediately".to_owned()),
    }
}

use std::future::Future;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn playbook_file(steps: Vec<Step>) -> PlaybookFile {
        PlaybookFile {
            name: "test".to_owned(),
            steps,
        }
    }

    fn step(key: &str, kind: NodeKind, label: &str) -> Step {
        Step {
            key: key.to_owned(),
            kind,
            label: label.to_owned(),
            depends_on: Vec::new(),
            budget_tokens: 0,
            max_attempts: 3,
            prompt: None,
            command: None,
            watch: Vec::new(),
            timeout_secs: Some(2),
            question: None,
        }
    }

    #[allow(dead_code)]
    fn context(_root: &Path) -> RunContext<'static> {
        unreachable!()
    }

    #[test]
    fn load_rejects_invalid_playbooks_through_the_compiler() {
        let dir = std::env::temp_dir().join(format!(
            "wf-load-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cycle.json");
        std::fs::write(
            &path,
            r#"{"name":"cycle","steps":[
                {"key":"a","kind":"task","depends_on":["b"]},
                {"key":"b","kind":"task","depends_on":["a"]}]}"#,
        )
        .unwrap();
        match load_playbook(&path) {
            Err(WorkflowError::Compiler(scheduler::playbook::PlaybookError::DependencyCycle)) => {}
            other => panic!("expected a cycle error, got {other:?}"),
        }
        let missing_command = dir.join("missing.json");
        std::fs::write(
            &missing_command,
            r#"{"name":"m","steps":[{"key":"v","kind":"verification"}]}"#,
        )
        .unwrap();
        match load_playbook(&missing_command) {
            Err(WorkflowError::InvalidStep { key, reason }) => {
                assert_eq!(key, "v");
                assert!(reason.contains("command"));
            }
            other => panic!("expected a missing-command error, got {other:?}"),
        }
        // An empty command is no command; an empty label is the key.
        let empty_command = dir.join("empty.json");
        std::fs::write(
            &empty_command,
            r#"{"name":"e","steps":[{"key":"v","kind":"verification","command":""}]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_playbook(&empty_command),
            Err(WorkflowError::InvalidStep { .. })
        ));
        let empty_label = dir.join("label.json");
        std::fs::write(
            &empty_label,
            r#"{"name":"l","steps":[{"key":"t","kind":"task","label":""}]}"#,
        )
        .unwrap();
        let (loaded, _) = load_playbook(&empty_label).expect("loads");
        assert_eq!(loaded.steps[0].label, "t");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_retrying_dependency_keeps_its_dependents_waiting_rather_than_cancelling_them() {
        // `build` fails once with attempts left. Its dependent `check` must
        // wait for the retry, not be cancelled on the first failure — the
        // Runtime Graph's rule, and the only reading under which "retries
        // in place" is true for a step that has dependents.
        let dir = std::env::temp_dir().join(format!(
            "wf-retry-dep-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![step("build", NodeKind::Task, "build"), {
            let mut s = step("check", NodeKind::Verification, "check");
            s.depends_on = vec!["build".into()];
            s.command = Some("true".into());
            s
        }]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        let builds = Arc::new(AtomicUsize::new(0));
        let builds_for_agent = Arc::clone(&builds);
        let agent: AgentStepFn = Arc::new(move |_key, _task| {
            if builds_for_agent.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("transient".into())
            } else {
                Ok("built".into())
            }
        });
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("ok".into()));
        let human: HumanWaitFn = Arc::new(|_s, _st| Err("none".into()));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 4,
            events: None,
        };
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(state.steps.get("check"), Some(&StepState::Succeeded));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_retryable_failure_waits_for_its_dependencies_like_any_other_step() {
        // `down` failed with attempts left while `up`, its dependency, was
        // reset to `Pending` (a resume after its watched files changed).
        // `down` is not ready until `up` succeeds again.
        let dir = std::env::temp_dir().join(format!(
            "wf-retry-wait-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![step("up", NodeKind::Task, "up"), {
            let mut s = step("down", NodeKind::Task, "down");
            s.depends_on = vec!["up".into()];
            // One attempt left: starting too early would be its last.
            s.max_attempts = 2;
            s
        }]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        state.steps.insert(
            "down".into(),
            StepState::Failed {
                reason: "earlier".into(),
                attempts: 1,
            },
        );
        state.attempts.insert("down".into(), 1);
        // `up` takes a moment; `down` reports whether `up` had finished
        // when it started. Re-queued alongside `up` (the old rule) it
        // would start first.
        let up_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let up_done_for_agent = Arc::clone(&up_done);
        let agent: AgentStepFn = Arc::new(move |key, _task| {
            if key == "up" {
                std::thread::sleep(std::time::Duration::from_millis(200));
                up_done_for_agent.store(true, Ordering::SeqCst);
                Ok("up done".into())
            } else if up_done_for_agent.load(Ordering::SeqCst) {
                Ok("down ran after up".into())
            } else {
                Err("down started before up finished".into())
            }
        });
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("ok".into()));
        let human: HumanWaitFn = Arc::new(|_s, _st| Err("none".into()));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 4,
            events: None,
        };
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        assert_eq!(
            state.results.get("down").map(String::as_str),
            Some("down ran after up")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_means_this_invocation_and_an_off_resume_abandons_a_verified_record() {
        let dir = std::env::temp_dir().join(format!(
            "wf-fresh-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![{
            let mut s = step("check", NodeKind::Verification, "check");
            s.command = Some("true".into());
            s
        }]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        state.orchestration = Some(crate::workflow_verified::OrchestrationRecord {
            mode: "verified".into(),
            session: "s".into(),
            graph_id: "g".into(),
            task_id: "t".into(),
            state: "Implementing".into(),
            accepted: false,
        });
        let agent: AgentStepFn = Arc::new(|_k, _t| Ok("n/a".into()));
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("checks pass".into()));
        let human: HumanWaitFn = Arc::new(|_s, _st| Err("none".into()));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 1,
            events: None,
        };
        let outcome = execute_run(
            &playbook,
            state,
            &context,
            Arc::clone(&agent),
            Arc::clone(&commands),
            Arc::clone(&human),
            &cancel,
            None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        let record = state.orchestration.as_ref().expect("kept as a pointer");
        assert!(record.state.starts_with("abandoned"), "{}", record.state);
        assert!(!record.accepted);
        // The check passed last time; this invocation ran nothing, so the
        // run is finished but not verified.
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(
            outcome,
            RunOutcome::CompletedUnverified {
                unmet: vec!["check".into()]
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diamond_runs_dependent_and_independent_steps_and_verifies() {
        let dir = std::env::temp_dir().join(format!(
            "wf-diamond-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![
            step("start", NodeKind::Goal, "start"),
            {
                let mut s = step("left", NodeKind::Task, "left");
                s.depends_on = vec!["start".into()];
                s
            },
            {
                let mut s = step("right", NodeKind::Task, "right");
                s.depends_on = vec!["start".into()];
                s
            },
            {
                let mut s = step("check", NodeKind::Verification, "check");
                s.depends_on = vec!["left".into(), "right".into()];
                s.command = Some("true".into());
                s
            },
        ]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_agent = Arc::clone(&ran);
        let agent: AgentStepFn = Arc::new(move |key, _task| {
            ran_agent.fetch_add(1, Ordering::SeqCst);
            Ok(format!("done: {key}"))
        });
        let commands: CommandStepFn = Arc::new(|_command, _timeout| Ok("ok".to_owned()));
        let human: HumanWaitFn = Arc::new(|_step, _state| Err("no human steps here".to_owned()));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 4,
            events: None,
        };
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        assert_eq!(ran.load(Ordering::SeqCst), 3, "start, left, right");
        assert_eq!(state.steps.get("check"), Some(&StepState::Succeeded));
        assert_eq!(state.fresh_verification.get("check"), Some(&true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_step_retries_then_succeeds_without_replaying_completed_steps() {
        let dir = std::env::temp_dir().join(format!(
            "wf-retry-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![step("first", NodeKind::Task, "first"), {
            let mut s = step("flaky", NodeKind::Task, "flaky");
            s.depends_on = vec!["first".into()];
            s.max_attempts = 2;
            s
        }]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        let first_runs = Arc::new(AtomicUsize::new(0));
        let flaky_runs = Arc::new(AtomicUsize::new(0));
        let first_for_agent = Arc::clone(&first_runs);
        let flaky_for_agent = Arc::clone(&flaky_runs);
        let agent: AgentStepFn = Arc::new(move |key, _task| {
            if key == "first" {
                first_for_agent.fetch_add(1, Ordering::SeqCst);
                Ok("first done".into())
            } else {
                flaky_for_agent.fetch_add(1, Ordering::SeqCst);
                if flaky_for_agent.load(Ordering::SeqCst) == 1 {
                    Err("transient failure".into())
                } else {
                    Ok("recovered".into())
                }
            }
        });
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("ok".into()));
        let human: HumanWaitFn = Arc::new(|_s, _st| Err("none".into()));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 4,
            events: None,
        };
        let outcome = execute_run(
            &playbook,
            state,
            &context,
            Arc::clone(&agent),
            Arc::clone(&commands),
            Arc::clone(&human),
            &cancel,
            None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        assert_eq!(flaky_runs.load(Ordering::SeqCst), 2, "one retry");
        // Resuming the completed run replays nothing: every step is
        // terminal and the closures must not be invoked again.
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        assert_eq!(first_runs.load(Ordering::SeqCst), 1);
        assert_eq!(flaky_runs.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_approval_step_pauses_and_resumes_after_a_decision() {
        let dir = std::env::temp_dir().join(format!(
            "wf-pause-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![
            step("build", NodeKind::Task, "build"),
            {
                let mut s = step("gate", NodeKind::Approval, "ship it?");
                s.depends_on = vec!["build".into()];
                s
            },
            {
                let mut s = step("verify", NodeKind::Verification, "verify");
                s.depends_on = vec!["gate".into()];
                s.command = Some("true".into());
                s
            },
        ]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        let agent: AgentStepFn = Arc::new(|_key, _task| Ok("built".into()));
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("ok".into()));
        let human: HumanWaitFn = Arc::new(|step, _state| Ok(format!("wait-{}", step.key)));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 4,
            events: None,
        };
        let outcome = execute_run(
            &playbook,
            state,
            &context,
            Arc::clone(&agent),
            Arc::clone(&commands),
            Arc::clone(&human),
            &cancel,
            None,
        );
        assert_eq!(
            outcome,
            RunOutcome::Paused {
                key: "gate".into(),
                wait_token: "wait-gate".into(),
            }
        );
        assert_eq!(state.paused_on.as_deref(), Some("gate"));
        // The human approves: the resolve command's transition.
        state.steps.insert("gate".into(), StepState::Succeeded);
        state.paused_on = None;
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pause_leaves_the_other_ready_steps_pending_for_the_resume() {
        // `gate` shares its ready batch with `left` and `right`. Pausing on
        // the gate must not strand the siblings as `Running`: they never
        // started, and a resume has to run them.
        let dir = std::env::temp_dir().join(format!(
            "wf-pause-siblings-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![
            step("left", NodeKind::Task, "left"),
            step("gate", NodeKind::Approval, "ship it?"),
            step("right", NodeKind::Task, "right"),
            {
                let mut s = step("verify", NodeKind::Verification, "verify");
                s.depends_on = vec!["left".into(), "gate".into(), "right".into()];
                s.command = Some("true".into());
                s
            },
        ]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_agent = Arc::clone(&ran);
        let agent: AgentStepFn = Arc::new(move |key, _task| {
            ran_agent.fetch_add(1, Ordering::SeqCst);
            Ok(format!("done: {key}"))
        });
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("ok".into()));
        let human: HumanWaitFn = Arc::new(|step, _state| Ok(format!("wait-{}", step.key)));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 4,
            events: None,
        };
        let outcome = execute_run(
            &playbook,
            state,
            &context,
            Arc::clone(&agent),
            Arc::clone(&commands),
            Arc::clone(&human),
            &cancel,
            None,
        );
        assert!(matches!(outcome, RunOutcome::Paused { .. }), "{outcome:?}");
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "nothing ran before the pause"
        );
        assert_eq!(state.steps.get("left"), Some(&StepState::Pending));
        assert_eq!(state.steps.get("right"), Some(&StepState::Pending));
        state.steps.insert("gate".into(), StepState::Succeeded);
        state.paused_on = None;
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            2,
            "left and right ran on resume"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completed_verification_is_invalidated_when_watched_files_change() {
        let dir = std::env::temp_dir().join(format!(
            "wf-inval-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        std::fs::write(dir.join("lib.rs"), "fn a() {}").unwrap();
        let playbook = playbook_file(vec![{
            let mut s = step("check", NodeKind::Verification, "check");
            s.command = Some("true".into());
            s.watch = vec!["lib.rs".into()];
            s
        }]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        let agent: AgentStepFn = Arc::new(|_k, _t| Ok("n/a".into()));
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("checks pass".into()));
        let human: HumanWaitFn = Arc::new(|_s, _st| Err("none".into()));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 1,
            events: None,
        };
        let outcome = execute_run(
            &playbook,
            state,
            &context,
            Arc::clone(&agent),
            Arc::clone(&commands),
            Arc::clone(&human),
            &cancel,
            None,
        );
        assert_eq!(outcome, RunOutcome::Verified);
        // The watched file changes: the recorded evidence is stale, and the
        // resume path must invalidate the verification before continuing.
        std::fs::write(dir.join("lib.rs"), "fn a() { /* changed */ }").unwrap();
        let mut resumed = load_run(&root, &playbook, &state.run_id).expect("loads");
        assert_eq!(resumed.steps.get("check"), Some(&StepState::Pending));
        assert!(!resumed.fresh_verification.contains_key("check"));
        let outcome = execute_run(
            &playbook,
            &mut resumed,
            &context,
            agent,
            commands,
            human,
            &cancel,
            None,
        );
        assert_eq!(
            outcome,
            RunOutcome::Verified,
            "re-verified against the new tree"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resume_that_runs_nothing_persists_the_cleared_freshness() {
        // A verification step passed last time; a resume that finds nothing
        // ready must still rewrite the file so `--status`/`load_run` do not
        // read a stale `[verified fresh]`.
        let dir = std::env::temp_dir().join(format!(
            "wf-persist-fresh-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.clone();
        let playbook = playbook_file(vec![{
            let mut s = step("check", NodeKind::Verification, "check");
            s.command = Some("true".into());
            s
        }]);
        let state = &mut RunState::new(new_run_id(), &playbook, Path::new("test.json"));
        // The finished state a prior invocation saved.
        state.steps.insert("check".into(), StepState::Succeeded);
        state.fresh_verification.insert("check".into(), true);
        save_run(&root, state).unwrap();
        let agent: AgentStepFn = Arc::new(|_k, _t| Ok("n/a".into()));
        let commands: CommandStepFn = Arc::new(|_c, _t| Ok("ok".into()));
        let human: HumanWaitFn = Arc::new(|_s, _st| Err("none".into()));
        let cancel = agent_runtime::CancellationToken::new();
        let context = RunContext {
            root: &root,
            trusted: true,
            max_parallel: 1,
            events: None,
        };
        let outcome = execute_run(
            &playbook, state, &context, agent, commands, human, &cancel, None,
        );
        assert_eq!(
            outcome,
            RunOutcome::CompletedUnverified {
                unmet: vec!["check".into()]
            }
        );
        // The persisted file — not just the in-memory state — is fresh-free.
        let reloaded = load_run(&root, &playbook, &state.run_id).expect("reload");
        assert!(
            !reloaded.fresh_verification.contains_key("check"),
            "the file still says fresh: {:?}",
            reloaded.fresh_verification
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_step_uncancels_the_dependents_a_final_failure_cancelled() {
        // Diamond: `a` (final failure) → `b` and `c` → `d`. `c` already
        // succeeded (independent of the failure); `b` and `d` were
        // cancelled. `--retry a` must restore `a`, `b`, `d` — the steps the
        // failure stopped — and leave the succeeded `c` alone.
        let mut state = RunState::new(new_run_id(), &playbook_file(vec![]), Path::new("test.json"));
        let dep = |key: &str, on: &[&str]| {
            let mut s = step(key, NodeKind::Task, key);
            s.depends_on = on.iter().map(|d| (*d).to_owned()).collect();
            s
        };
        let playbook = playbook_file(vec![
            step("a", NodeKind::Task, "a"),
            dep("b", &["a"]),
            dep("c", &[]),
            dep("d", &["b", "c"]),
        ]);
        state.steps.insert(
            "a".into(),
            StepState::Failed {
                reason: "final".into(),
                attempts: 3,
            },
        );
        state.attempts.insert("a".into(), 3);
        state.steps.insert("b".into(), StepState::Cancelled);
        state.steps.insert("c".into(), StepState::Succeeded);
        state.steps.insert("d".into(), StepState::Cancelled);
        reset_step(&mut state, &playbook, "a").expect("reset");
        assert_eq!(state.steps.get("a"), Some(&StepState::Pending));
        assert_eq!(
            state.steps.get("b"),
            Some(&StepState::Pending),
            "un-cancelled"
        );
        assert_eq!(
            state.steps.get("d"),
            Some(&StepState::Pending),
            "transitively, through b"
        );
        assert_eq!(
            state.steps.get("c"),
            Some(&StepState::Succeeded),
            "a succeeded step is never touched"
        );
        assert!(!state.attempts.contains_key("a"));
        // An unknown step is an error, not a silent no-op.
        reset_step(&mut state, &playbook, "missing").unwrap_err();
    }

    #[test]
    fn glob_matching_covers_star_and_nested_paths() {
        assert!(glob_segment("*.rs", "lib.rs"));
        assert!(!glob_segment("*.rs", "src/lib.rs"));
        assert!(glob_segment("src/*.rs", "src/lib.rs"));
        assert!(segment_glob(
            Path::new("src/*.rs"),
            Path::new("src/main.rs")
        ));
        assert!(!segment_glob(
            Path::new("src/*.rs"),
            Path::new("tests/x.rs")
        ));
        let dir = std::env::temp_dir().join(format!(
            "wf-glob-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "y").unwrap();
        let mut out = Vec::new();
        collect_glob(&dir, Path::new("src/*.rs"), Path::new(""), &mut out);
        assert_eq!(out, vec![PathBuf::from("src/a.rs")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
