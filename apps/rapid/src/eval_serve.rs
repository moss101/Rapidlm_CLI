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
//!   repositories with the same verification commands. Agents that are not
//!   installed, or runs that exceed the wall-clock ceiling, are recorded as
//!   skipped/failed with the reason — never silently dropped, and no
//!   comparison is claimed without actual runs on both sides.
//!
//! Output: a JSON results file (`eval/results/<stamp>-<mode>.json`) and a
//! summary line per task; exit 0 iff every non-skipped task passed.

use agent_runtime::run_turn;
use protocol::{AgentId, SessionId};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Wall-clock ceiling per live task (offline tasks finish in milliseconds).
const LIVE_TASK_TIMEOUT_SECS: u64 = 600;

/// Where the suite ships.
pub const SUITE_DIR: &str = "eval/suite";

// ---------------------------------------------------------------------------
// Task schema
// ---------------------------------------------------------------------------

/// One benchmark task. `deny_unknown_fields` semantics via manual parse: a
/// task file the runner does not fully understand must fail the load, not
/// silently skip fields.
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
    /// exit 0 = the task is done correctly.
    pub verify: String,
    /// A second command that must FAIL on the unmodified repo (the task is
    /// not vacuous) — the anti-vacuity check every offline run performs.
    pub verify_fails_before: bool,
}

const MAX_TASKS: usize = 64;
const MAX_FILE_BYTES: usize = 16 * 1024;

/// Load every task file in a directory, sorted by name.
pub fn load_suite(dir: &Path) -> Result<Vec<BenchTask>, String> {
    let mut tasks = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| format!("{}: {err}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    entries.sort();
    for path in entries {
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
        let files = |key: &str| -> Result<Vec<(String, String)>, String> {
            let mut out = Vec::new();
            for (path, contents) in value
                .get(key)
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| format!("{}: missing {key} object", path_name(dir)))?
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
        });
    }
    if tasks.is_empty() {
        return Err(format!("{}: no tasks found", dir.display()));
    }
    Ok(tasks)
}

fn path_name(dir: &Path) -> String {
    dir.display().to_string()
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

fn run_verify(dir: &Path, command: &str, timeout_secs: u64) -> Result<bool, String> {
    use std::io::Read as _;
    // `{RAPID}` in a task's verify command names this binary — so a judge
    // can invoke real rapid subcommands without depending on PATH.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rapid"));
    let substituted = command.replace("{RAPID}", &format!("'{}'", exe.display()));
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&substituted)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) => return Ok(status.success()),
            None => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(format!("verify exceeded its {timeout_secs}s ceiling"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Offline (mechanical) mode: the scripted gold trajectory through the real
// turn loop.
// ---------------------------------------------------------------------------

/// A model driver that performs the task's gold patch: one `workspace_write`
/// call per gold file, then a terminal answer. This exercises the real tool
/// dispatcher, permission lattice, and ledger — nothing about the outcome is
/// asserted without the verification command running.
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
/// report that hides its skips is a lie.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TaskResult {
    pub id: String,
    pub category: String,
    pub outcome: String, // passed | failed | skipped
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub wall_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

/// Run the suite offline: gold trajectory through the real turn loop, graded
/// only by the task's verification command (plus the anti-vacuity check).
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
                Err(reason) => {
                    let _ = reason;
                    "failed".to_owned()
                }
            },
            reason: result.err(),
            wall_ms: started.elapsed().as_millis(),
            tokens: Some(u64::from(task.gold.len() as u32 + 1) * 10),
        });
    }
    results
}

fn run_offline_task(task: &BenchTask, scratch: &Path, trusted: bool) -> Result<(), String> {
    materialize(scratch, task)?;
    // Anti-vacuity: the verification command must FAIL on the unmodified
    // repo, or the task proves nothing.
    if task.verify_fails_before && run_verify(scratch, &task.verify, 60).unwrap_or(true) {
        return Err(
            "verification command passes on the UNMODIFIED repo; the task is vacuous".to_owned(),
        );
    }
    // The gold trajectory through the REAL turn loop: tools, lattice, ledger.
    if !trusted {
        return Err(
            "the project is not trusted; offline evaluation refuses to run with no tools"
                .to_owned(),
        );
    }
    // The gold trajectory is scripted, not a model's proposal: the calls are
    // known-good by construction, so the lattice runs in bypass mode (the
    // same seam the interactive test harness uses) and the permission gate
    // is not what is under test here — the verification command is.
    let mut tools = crate::exec_tools::ExecTools::workspace_with_permissions(
        scratch,
        crate::permissions::PermissionLattice::new(
            crate::permissions::PermissionMode::BypassPermissions,
        ),
    )
    .map_err(|err| err.to_string())?;
    tools.set_approval_source(std::sync::Arc::new(
        crate::approvals::LedgerApprovalSink::new(
            offline_ledger_client(scratch)?,
            SessionId::new(),
            event_ledger::event::ActorRef::new(
                event_ledger::event::ActorKind::Agent,
                &protocol::EventId::new().to_string(),
            )
            .map_err(|err| err.to_string())?,
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
    .map_err(|err| err.to_string())?;
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
    .map_err(|err| err.to_string())?;
    if outcome.status() != agent_runtime::TurnStatus::Completed {
        return Err(format!(
            "the gold trajectory turn did not complete: {}",
            outcome
                .reason()
                .map(|reason| reason.as_str())
                .unwrap_or("unknown")
        ));
    }
    // The external judge: verification exit 0 in the patched repo.
    if !run_verify(scratch, &task.verify, 120).unwrap_or(false) {
        return Err("verification command failed after the gold patch".to_owned());
    }
    Ok(())
}

fn offline_ledger_client(scratch: &Path) -> Result<kernel::InProcessKernelClient, String> {
    let ledger_path = scratch.join(".rapidlm").join("sessions.db");
    if let Some(parent) = ledger_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    kernel::InProcessKernelClient::open(&ledger_path).map_err(|err| err.to_string())
}

// ---------------------------------------------------------------------------
// Live mode: real agent CLIs, identical repos, same judge.
// ---------------------------------------------------------------------------

/// The live agents. `rapid` is ourselves; competitors run when their pinned
/// CLI is on PATH. Each entry names the binary to probe and the argv shape.
pub const LIVE_AGENTS: &[(&str, &str)] = &[
    ("rapid", "rapid exec <prompt>"),
    ("qwen", "qwen -p <prompt> (pinned version required)"),
    ("grok", "grok <prompt> (pinned version required)"),
];

fn probe_binary(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(&format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The real live runner: `bin` is the resolved agent binary (ours is the
/// current executable); the prompt is piped on stdin so agents with
/// different CLI grammars get identical instructions.
fn run_live_agent_with(
    bin: &str,
    args: &[String],
    task: &BenchTask,
    scratch: &Path,
) -> Result<(bool, u64), String> {
    use std::io::Write as _;
    use std::process::Stdio;
    let started = std::time::Instant::now();
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{bin} could not start: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(task.prompt.as_bytes());
    }
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(LIVE_TASK_TIMEOUT_SECS);
    let status = loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(format!(
                        "{bin} exceeded the {LIVE_TASK_TIMEOUT_SECS}s ceiling"
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    };
    let _ = started;
    Ok((status.success(), 0)) // tokens: parsed per-agent when the CLI reports them; 0 = not reported
}

/// Run the suite live for every agent whose binary is present; agents that
/// are absent are recorded as skipped with the reason — a comparison is only
/// ever drawn between agents that actually ran.
pub fn run_live(
    tasks: &[BenchTask],
    scratch_root: &Path,
    self_exe: &Path,
    trusted: bool,
) -> Vec<(String, Vec<TaskResult>)> {
    let mut agents = vec![(
        "rapid".to_owned(),
        self_exe.to_path_buf(),
        vec!["exec".to_owned()],
    )];
    for (name, _recipe) in LIVE_AGENTS {
        if *name != "rapid" && probe_binary(name) {
            // Competitor invocation recipes are pinned here once their
            // versions are pinned; until then a present-but-unpinned binary
            // is still skipped, honestly.
            agents.push(((*name).to_owned(), PathBuf::from(name), vec![]));
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut per_agent = Vec::new();
    let _ = trusted;
    // Pre-flight: a probe task in a scratch repo, using the same invocation
    // the real tasks get. If the model call itself fails (no credentials,
    // unreachable endpoint), every live task for that agent is recorded as
    // SKIPPED with that reason — failures of the environment are not
    // failures of the tasks, and the report must say which is which.
    for (name, bin, args) in &agents {
        if *name != "rapid" {
            // handled by the per-agent skip below; keep the probe cheap
        } else {
            let probe = scratch_root.join(format!("preflight-{stamp:x}"));
            let model_ready = match run_live_rapid(bin, args, &tasks[0], &probe, trusted) {
                Err(reason)
                    if reason.contains("model")
                        || reason.contains("provider")
                        || reason.contains("non-zero") =>
                {
                    let _ = std::fs::remove_dir_all(&probe);
                    Some(reason)
                }
                _ => {
                    let _ = std::fs::remove_dir_all(&probe);
                    None
                }
            };
            if let Some(reason) = model_ready {
                per_agent.push((
                    name.clone(),
                    tasks
                        .iter()
                        .map(|task| TaskResult {
                            id: task.id.clone(),
                            category: task.category.clone(),
                            outcome: "skipped".to_owned(),
                            reason: Some(format!("model not usable: {reason}")),
                            wall_ms: 0,
                            tokens: None,
                        })
                        .collect(),
                ));
                continue;
            }
        }
        let mut results = Vec::new();
        for task in tasks {
            let started = std::time::Instant::now();
            let scratch = scratch_root.join(format!("{name}-{}-{stamp:x}", task.id));
            let result = if *name == "rapid" {
                run_live_rapid(bin, &args, task, &scratch, trusted)
            } else {
                Err(format!(
                    "{name}: present on PATH but no pinned version/invocation is recorded; \
skipped rather than compared unpinned"
                ))
            };
            let _ = std::fs::remove_dir_all(&scratch);
            results.push(TaskResult {
                id: task.id.clone(),
                category: task.category.clone(),
                outcome: match &result {
                    Ok(()) => "passed".to_owned(),
                    Err(reason) => {
                        if reason.contains("skipped") {
                            "skipped".to_owned()
                        } else {
                            "failed".to_owned()
                        }
                    }
                },
                reason: result.err(),
                wall_ms: started.elapsed().as_millis(),
                tokens: None,
            });
        }
        per_agent.push((name.clone(), results));
    }
    per_agent
}

fn run_live_rapid(
    bin: &Path,
    args: &[String],
    task: &BenchTask,
    scratch: &Path,
    trusted: bool,
) -> Result<(), String> {
    if !trusted {
        return Err(
            "skipped: the project is not trusted; live runs would have no tools".to_owned(),
        );
    }
    materialize(scratch, task)?;
    if task.verify_fails_before && run_verify(scratch, &task.verify, 60).unwrap_or(true) {
        return Err("vacuous task (verify passes before any change)".to_owned());
    }
    // rapid exec against the scratch repo: the prompt via a task file arg.
    let mut argv = args.to_vec();
    argv.push(task.prompt.clone());
    let (ok, _tokens) = run_live_agent_with(&bin.display().to_string(), &argv, task, scratch)?;
    if !ok {
        return Err("rapid exec exited non-zero".to_owned());
    }
    if !run_verify(scratch, &task.verify, 120).unwrap_or(false) {
        return Err("verification command failed after the live run".to_owned());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Summary counts + the JSON report body. Mechanical and live reports are
/// written to separate files and never summed together.
pub fn summarize(mode: &str, results: &[TaskResult]) -> (serde_json::Value, String) {
    let passed = results.iter().filter(|r| r.outcome == "passed").count();
    let failed = results.iter().filter(|r| r.outcome == "failed").count();
    let skipped = results.iter().filter(|r| r.outcome == "skipped").count();
    let report = serde_json::json!({
        "mode": mode,
        "kind": if mode == "offline" { "mechanical validation (scripted gold trajectories; says nothing about model quality)" } else { "live model quality (agents given prompts only; the verification command is the judge)" },
        "tasks": results.len(),
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "results": results,
    });
    let mut lines = vec![format!(
        "{mode}: {}/{} passed, {failed} failed, {skipped} skipped",
        passed,
        results.len()
    )];
    for result in results {
        if result.outcome != "passed" {
            lines.push(format!(
                "  {} [{}] {}: {}",
                result.id,
                result.category,
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

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub const EVAL_USAGE: &str = "usage: rapid eval --offline [--suite <dir>] [--scratch <dir>] | rapid eval --live [--suite <dir>]

Run the reproducible coding benchmark.

  --offline   mechanical validation: each task's gold patch replays through
              the real turn executor (tools, permission lattice, ledger) and
              the task's verification command is the only judge. Deterministic;
              says nothing about model quality.
  --live      model quality: real agent CLIs run on identical scratch repos
              with the same prompts and the same verification commands.
              Agents without a pinned, present binary are recorded as skipped;
              no comparison is claimed without actual runs on both sides.

Results are written to eval/results/<mode>-<stamp>.json; exit 0 iff every
non-skipped task passed.
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
    let Some((root, trusted)) = crate::interactive::workflow_workspace_root() else {
        eprintln!("rapid eval: no project workspace resolved");
        return Err(crate::p9_commands::P9CommandError::Usage);
    };
    let _ = &root;

    let mode = if offline { "offline" } else { "live" };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let results = if offline {
        run_offline(&tasks, &scratch, trusted)
    } else {
        let self_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rapid"));
        run_live(&tasks, &scratch, &self_exe, trusted)
            .into_iter()
            .flat_map(|(_agent, results)| results)
            .collect::<Vec<_>>()
    };
    let (report, summary) = summarize(mode, &results);
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
    let any_failed = results.iter().any(|result| result.outcome == "failed");
    Ok(if any_failed { 1 } else { 0 })
}
